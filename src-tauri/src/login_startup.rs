use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn installed_bundle(executable: &Path, home: &Path, debug: bool) -> Option<PathBuf> {
    if debug {
        return None;
    }
    [PathBuf::from("/Applications"), home.join("Applications")]
        .into_iter()
        .map(|root| root.join("Tokenscope.app"))
        .find(|bundle| executable == bundle.join("Contents/MacOS/tokenscope"))
}

fn current_registration() -> Result<Option<Registration>> {
    let home = dirs::home_dir().ok_or("Home directory is unavailable")?;
    let executable = std::env::current_exe()?.canonicalize()?;
    Ok(
        installed_bundle(&executable, &home, cfg!(debug_assertions)).map(|bundle| Registration {
            path: home.join("Library/LaunchAgents/Tokenscope.plist"),
            bundle,
        }),
    )
}

pub fn available() -> bool {
    matches!(current_registration(), Ok(Some(_)))
}

pub fn is_enabled() -> Result<bool> {
    match current_registration()? {
        Some(registration) => registration.is_enabled(),
        None => Ok(false),
    }
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    current_registration()?
        .ok_or("Launch at Login requires a release app installed in Applications")?
        .set_enabled(enabled)
}

struct Registration {
    path: PathBuf,
    bundle: PathBuf,
}

impl Registration {
    fn expected(&self) -> Result<plist::Value> {
        let bundle = self
            .bundle
            .to_str()
            .ok_or("Application path is not UTF-8")?;
        let mut dict = plist::Dictionary::new();
        dict.insert("Label".into(), "Tokenscope".into());
        dict.insert("RunAtLoad".into(), true.into());
        // LaunchServices applies normal application-launch policy. Direct execution
        // by a managed LaunchAgent can reject an otherwise launchable ad-hoc app.
        dict.insert(
            "ProgramArguments".into(),
            plist::Value::Array(
                ["/usr/bin/open", "-g", bundle]
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            ),
        );
        Ok(plist::Value::Dictionary(dict))
    }

    fn is_enabled(&self) -> Result<bool> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        let expected = self.expected()?;
        Ok(plist::Value::from_reader(std::io::Cursor::new(bytes))
            .is_ok_and(|value| value == expected))
    }

    fn set_enabled(&self, enabled: bool) -> Result<()> {
        if !enabled {
            return match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error.into()),
            };
        }
        if self.is_enabled()? {
            return Ok(());
        }
        let parent = self.path.parent().ok_or("Missing launch-agent directory")?;
        std::fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        self.expected()?.to_writer_xml(temporary.as_file_mut())?;
        temporary.persist(&self.path)?;
        // Do not bootout a legacy job here: it may own this running process.
        // launchd reads the migrated registration at the next login.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Registration) {
        let temp = tempfile::tempdir().unwrap();
        let registration = Registration {
            path: temp.path().join("LaunchAgents/Tokenscope.plist"),
            bundle: temp.path().join("Apps & Tools/Tokenscope.app"),
        };
        (temp, registration)
    }

    #[test]
    fn only_installed_release_builds_manage_login_startup() {
        let home = Path::new("/Users/test");
        let system = Path::new("/Applications/Tokenscope.app/Contents/MacOS/tokenscope");
        let user = home.join("Applications/Tokenscope.app/Contents/MacOS/tokenscope");
        assert_eq!(
            installed_bundle(system, home, false),
            Some(PathBuf::from("/Applications/Tokenscope.app"))
        );
        assert_eq!(
            installed_bundle(&user, home, false),
            Some(home.join("Applications/Tokenscope.app"))
        );
        assert!(installed_bundle(system, home, true).is_none());
        for path in [
            "/Users/test/develop/repo/target/debug/tokenscope",
            "/Users/test/develop/repo/target/release/bundle/macos/Tokenscope.app/Contents/MacOS/tokenscope",
            "/Volumes/Tokenscope/Tokenscope.app/Contents/MacOS/tokenscope",
        ] {
            assert!(installed_bundle(Path::new(path), home, false).is_none());
        }
    }

    #[test]
    fn registration_uses_launch_services_and_preserves_path_characters() {
        let (_temp, registration) = fixture();
        assert!(!registration.is_enabled().unwrap());
        registration.set_enabled(true).unwrap();
        let value = plist::Value::from_file(&registration.path).unwrap();
        let dict = value.as_dictionary().unwrap();
        let args: Vec<_> = dict["ProgramArguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_string().unwrap())
            .collect();
        assert_eq!(
            args,
            vec!["/usr/bin/open", "-g", registration.bundle.to_str().unwrap()]
        );
        assert_eq!(dict["Label"].as_string(), Some("Tokenscope"));
        assert_eq!(dict["RunAtLoad"].as_boolean(), Some(true));
        assert!(dict.get("Program").is_none());
        assert!(dict.get("KeepAlive").is_none());
        assert!(registration.is_enabled().unwrap());
        let modified = std::fs::metadata(&registration.path)
            .unwrap()
            .modified()
            .unwrap();
        registration.set_enabled(true).unwrap();
        assert_eq!(
            modified,
            std::fs::metadata(&registration.path)
                .unwrap()
                .modified()
                .unwrap()
        );
    }

    #[test]
    fn enabling_migrates_legacy_and_malformed_registrations() {
        let (_temp, registration) = fixture();
        std::fs::create_dir_all(registration.path.parent().unwrap()).unwrap();
        for contents in [
            "<plist version=\"1.0\"><dict><key>Label</key><string>Tokenscope</string><key>ProgramArguments</key><array><string>/old/Tokenscope.app/Contents/MacOS/tokenscope</string></array><key>RunAtLoad</key><true/></dict></plist>",
            "invalid plist",
        ] {
            std::fs::write(&registration.path, contents).unwrap();
            assert!(!registration.is_enabled().unwrap());
            registration.set_enabled(true).unwrap();
            assert!(registration.is_enabled().unwrap());
        }
    }

    #[test]
    fn disabling_removes_legacy_entries_and_is_idempotent() {
        let (_temp, registration) = fixture();
        std::fs::create_dir_all(registration.path.parent().unwrap()).unwrap();
        std::fs::write(&registration.path, "legacy registration").unwrap();
        registration.set_enabled(false).unwrap();
        assert!(!registration.path.exists());
        registration.set_enabled(false).unwrap();
        assert!(!registration.is_enabled().unwrap());
    }

    #[test]
    fn registration_errors_are_not_reported_as_success() {
        let (temp, registration) = fixture();
        std::fs::write(temp.path().join("LaunchAgents"), "not a directory").unwrap();
        assert!(registration.set_enabled(true).is_err());
        assert!(registration.is_enabled().is_err());
    }
}
