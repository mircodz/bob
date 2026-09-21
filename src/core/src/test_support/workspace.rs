use std::path::{Path, PathBuf};

pub(crate) struct TestDir(pub PathBuf);

impl TestDir {
    pub fn new() -> Self {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");
        let base = workspace.join("target/test-fixtures");
        std::fs::create_dir_all(&base).expect("test fixture directory");
        let base = base.canonicalize().expect("canonical fixture directory");
        assert!(
            base.starts_with(&workspace),
            "fixtures must remain in the workspace"
        );
        let mut random = [0; 16];
        getrandom::getrandom(&mut random).expect("fixture identifier");
        let name: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = base.join(format!("core-{}-{name}", std::process::id()));
        std::fs::create_dir(&path).expect("create fixture");
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TestDir {
    fn as_ref(&self) -> &Path {
        self.path()
    }
}

impl std::ops::Deref for TestDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.path()
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
