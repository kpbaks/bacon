use {
    crate::*,
    lazy_regex::regex_replace_all,
    rustc_hash::FxHashSet,
    std::{
        collections::HashMap,
        path::PathBuf,
    },
};

/// the description of the mission of bacon
/// after analysis of the args, env, and surroundings
#[derive(Debug)]
pub struct Mission<'s> {
    pub location_name: String,
    pub concrete_job_ref: ConcreteJobRef,
    pub execution_directory: PathBuf,
    pub package_directory: PathBuf,
    pub workspace_directory: Option<PathBuf>,
    pub job: Job,
    pub paths_to_watch: Vec<PathBuf>,
    pub settings: &'s Settings,
}

impl Mission<'_> {
    /// Return an Ignorer according to the job's settings
    pub fn ignorer(&self) -> IgnorerSet {
        let mut set = IgnorerSet::default();
        if self.job.apply_gitignore != Some(false) {
            match GitIgnorer::new(&self.package_directory) {
                Ok(git_ignorer) => {
                    set.add(Box::new(git_ignorer));
                }
                Err(e) => {
                    // might be normal, eg not in a git repo
                    debug!("Failed to initialise git ignorer: {e}");
                }
            }
        }
        if !self.job.ignore.is_empty() {
            let mut glob_ignorer = GlobIgnorer::default();
            for pattern in &self.job.ignore {
                if let Err(e) = glob_ignorer.add(pattern, &self.package_directory) {
                    warn!("Failed to add ignore pattern {pattern}: {e}");
                }
            }
            set.add(Box::new(glob_ignorer));
        }
        set
    }

    pub fn is_success(
        &self,
        report: &Report,
    ) -> bool {
        report.is_success(self.job.allow_warnings(), self.job.allow_failures())
    }

    pub fn make_absolute(
        &self,
        path: PathBuf,
    ) -> PathBuf {
        if path.is_absolute() {
            return path;
        }
        // There's a small mess here. Cargo tends to make paths relative
        // not to the package or work directory but to the workspace, contrary
        // to any sane tool. We have to guess.
        if let Some(workspace) = &self.workspace_directory {
            let workspace_joined = workspace.join(&path);
            if workspace_joined.exists() {
                return workspace_joined;
            }
        }
        self.package_directory.join(&path)
    }

    /// build (and doesn't call) the external cargo command
    pub fn get_command(&self) -> anyhow::Result<CommandBuilder> {
        let mut command = if self.job.expand_env_vars() {
            self.job
                .command
                .iter()
                .map(|token| {
                    regex_replace_all!(r"\$([A-Z0-9a-z_]+)", token, |whole: &str, name| {
                        match std::env::var(name) {
                            Ok(value) => value,
                            Err(_) => {
                                warn!("variable {whole} not found in env");
                                whole.to_string()
                            }
                        }
                    })
                    .to_string()
                })
                .collect()
        } else {
            self.job.command.clone()
        };

        if command.is_empty() {
            anyhow::bail!(
                "Empty command in job {}",
                self.concrete_job_ref.badge_label()
            );
        }

        let scope = &self.concrete_job_ref.scope;
        if scope.has_tests() && command.len() > 2 {
            let tests = if command[0] == "cargo" && command[1] == "test" {
                // Here we're going around a limitation of the vanilla cargo test:
                // it can only be scoped to one test
                &scope.tests[..1]
            } else {
                &scope.tests
            };
            for test in tests {
                command.push(test.to_string());
            }
        }

        let mut tokens = command.iter();
        let mut command = CommandBuilder::new(
            tokens.next().unwrap(), // implies a check in the job
        );
        command.with_stdout(self.job.need_stdout());
        let envs: HashMap<&String, &String> = self
            .settings
            .all_jobs
            .env
            .iter()
            .chain(self.job.env.iter())
            .collect();
        if !self.job.extraneous_args() {
            command.args(tokens);
            command.current_dir(&self.execution_directory);
            command.envs(envs);
            debug!("command: {:#?}", &command);
            return Ok(command);
        }

        let mut no_default_features_done = false;
        let mut features_done = false;
        let mut last_is_features = false;
        let mut tokens = tokens.chain(&self.settings.additional_job_args);
        let mut has_double_dash = false;
        for arg in tokens.by_ref() {
            if arg == "--" {
                // we'll defer addition of the following arguments to after
                // the addition of the features stuff, so that the features
                // arguments are given to the cargo command.
                has_double_dash = true;
                break;
            }
            if last_is_features {
                if self.settings.all_features {
                    debug!("ignoring features given along --all-features");
                } else {
                    features_done = true;
                    // arg is expected there to be the list of features
                    match (&self.settings.features, self.settings.no_default_features) {
                        (Some(features), false) => {
                            // we take the features of both the job and the args
                            command.arg("--features");
                            command.arg(merge_features(arg, features));
                        }
                        (Some(features), true) => {
                            // arg add features and remove the job ones
                            command.arg("--features");
                            command.arg(features);
                        }
                        (None, true) => {
                            // we pass no feature
                        }
                        (None, false) => {
                            // nothing to change
                            command.arg("--features");
                            command.arg(arg);
                        }
                    }
                }
                last_is_features = false;
            } else if arg == "--no-default-features" {
                no_default_features_done = true;
                last_is_features = false;
                command.arg(arg);
            } else if arg == "--features" {
                last_is_features = true;
            } else {
                command.arg(arg);
            }
        }
        if self.settings.no_default_features && !no_default_features_done {
            command.arg("--no-default-features");
        }
        if self.settings.all_features {
            command.arg("--all-features");
        }
        if !features_done {
            if let Some(features) = &self.settings.features {
                if self.settings.all_features {
                    debug!("not using features because of --all-features");
                } else {
                    command.arg("--features");
                    command.arg(features);
                }
            }
        }
        if has_double_dash {
            command.arg("--");
            for arg in tokens {
                command.arg(arg);
            }
        }
        command.current_dir(&self.execution_directory);
        command.envs(envs);
        debug!("command builder: {:#?}", &command);
        Ok(command)
    }

    pub fn kill_command(&self) -> Option<Vec<String>> {
        self.job.kill.clone()
    }

    /// whether we need stdout and not just stderr
    pub fn need_stdout(&self) -> bool {
        self.job
            .need_stdout
            .or(self.settings.all_jobs.need_stdout)
            .unwrap_or(false)
    }

    pub fn analyzer(&self) -> AnalyzerRef {
        self.job.analyzer.unwrap_or_default()
    }

    pub fn ignored_lines_patterns(&self) -> Option<&Vec<LinePattern>> {
        self.job
            .ignored_lines
            .as_ref()
            .or(self.settings.all_jobs.ignored_lines.as_ref())
            .filter(|p| !p.is_empty())
    }

    pub fn sound_player_if_needed(&self) -> Option<SoundPlayer> {
        if self.job.sound.is_enabled() {
            match SoundPlayer::new(self.job.sound.get_base_volume()) {
                Ok(sound_player) => Some(sound_player),
                Err(e) => {
                    warn!("Failed to initialise sound player: {e}");
                    None
                }
            }
        } else {
            None
        }
    }
}

fn merge_features(
    a: &str,
    b: &str,
) -> String {
    let mut features = FxHashSet::default();
    for feature in a.split(',') {
        features.insert(feature);
    }
    for feature in b.split(',') {
        features.insert(feature);
    }
    features.iter().copied().collect::<Vec<&str>>().join(",")
}
