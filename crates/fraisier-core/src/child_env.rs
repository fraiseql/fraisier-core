//! Environment variables no child process of fraisier may inherit.
//!
//! A child inherits fraisier's whole environment unless told otherwise. That is
//! wrong for a secret fraisier holds for itself: `[database].admin_url_env` names
//! a superuser DSN for fraisier's own restores, and a migration tool, a hook or a
//! notifier is someone else's code. Loading a config [`withhold`]s that name, and
//! every spawn site calls [`scrub`] before it adds the variables it means to
//! pass, so a variable a site sets on purpose still reaches its child.
//!
//! The set is process-wide because the spawn sites live in adapters that never
//! see the config; it only ever grows, and it holds names, never values.

use std::collections::BTreeSet;
use std::sync::{PoisonError, RwLock};

static WITHHELD: RwLock<BTreeSet<String>> = RwLock::new(BTreeSet::new());

/// Withhold the variable `name` from every child fraisier spawns from now on.
pub fn withhold(name: &str) {
    WITHHELD
        .write()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(name.to_owned());
}

/// Remove every withheld variable from `command`'s environment.
///
/// Call it before setting the variables the child should get: a later `env` of
/// the same name wins.
pub fn scrub(command: &mut std::process::Command) {
    for name in WITHHELD
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
    {
        command.env_remove(name);
    }
}

#[cfg(test)]
mod tests {
    use super::{scrub, withhold};

    #[test]
    fn a_withheld_variable_is_removed_from_a_child() {
        withhold("FRAISIER_CHILD_ENV_TEST_WITHHELD");
        let mut command = std::process::Command::new("true");
        scrub(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "FRAISIER_CHILD_ENV_TEST_WITHHELD" && value.is_none()),
            "{:?}",
            command.get_envs().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_variable_never_withheld_is_left_alone() {
        let mut command = std::process::Command::new("true");
        scrub(&mut command);
        assert!(!command
            .get_envs()
            .any(|(key, _)| key == "FRAISIER_CHILD_ENV_TEST_NEVER_WITHHELD"));
    }
}
