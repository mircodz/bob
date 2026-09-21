#[path = "../src/test_support/workspace.rs"]
mod workspace;

mod support {
    pub(crate) use super::workspace::TestDir;
}

#[path = "contracts/persistence.rs"]
mod persistence;
