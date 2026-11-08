//! Command selection and login policy, independent of terminals and storage.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMode {
    Interactive,
    Initialize,
    Service,
}

impl LoginMode {
    pub fn may_prompt(self) -> bool {
        self != Self::Service
    }

    pub fn allows_volatile_login(self) -> bool {
        self == Self::Interactive
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run(LoginMode),
    Init,
    Wait,
    Health,
    ServiceAttempt,
    ContainerInit,
    Backup(String),
    Password,
    Version,
    Help,
}

pub fn parse(args: &[String]) -> Result<Command, &'static str> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] | ["run"] => Ok(Command::Run(LoginMode::Interactive)),
        ["run", "--non-interactive"] => Ok(Command::Run(LoginMode::Service)),
        ["run", "--wait"] => Ok(Command::Wait),
        ["health"] => Ok(Command::Health),
        ["__service-attempt"] => Ok(Command::ServiceAttempt),
        ["__container-init"] => Ok(Command::ContainerInit),
        ["init"] => Ok(Command::Init),
        ["backup", output] if !output.is_empty() => Ok(Command::Backup((*output).to_owned())),
        ["password"] => Ok(Command::Password),
        ["--version" | "-V" | "version"] => Ok(Command::Version),
        ["--help" | "-h" | "help"] => Ok(Command::Help),
        _ => Err("Invalid arguments. Run `tutabridge --help` for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Result<Command, &'static str> {
        parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn desktop_defaults_to_interactive_and_service_requires_an_explicit_mode() {
        assert_eq!(command(&[]), Ok(Command::Run(LoginMode::Interactive)));
        assert_eq!(command(&["run"]), Ok(Command::Run(LoginMode::Interactive)));
        assert_eq!(
            command(&["run", "--non-interactive"]),
            Ok(Command::Run(LoginMode::Service))
        );
        assert_eq!(command(&["init"]), Ok(Command::Init));
    }

    #[test]
    fn unknown_or_extra_arguments_never_start_a_bridge() {
        for args in [
            vec!["rn"],
            vec!["run", "--non-interactiv"],
            vec!["init", "secret"],
            vec!["backup"],
            vec!["backup", "out", "extra"],
            vec!["password", "extra"],
        ] {
            assert!(command(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn initialization_requires_persistence_and_service_never_prompts() {
        assert!(LoginMode::Interactive.may_prompt());
        assert!(LoginMode::Interactive.allows_volatile_login());
        assert!(LoginMode::Initialize.may_prompt());
        assert!(!LoginMode::Initialize.allows_volatile_login());
        assert!(!LoginMode::Service.may_prompt());
        assert!(!LoginMode::Service.allows_volatile_login());
    }
}
