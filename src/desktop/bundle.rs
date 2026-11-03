//! macOS bundle builds. `claude.app` is sealed, so the original moves whole to
//! `claude.real.app`, its signature intact, and a small ad-hoc signed dense
//! bundle takes its name. The wrapper is built and signed aside first: Desktop
//! must only ever find a complete bundle, since a binary killed at exec makes
//! it re-download the engine.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Result;
use crate::error::{Context, Error};

use super::{BIN, BUNDLE, Outcome, REAL_BUNDLE, file_sha256, place, swap};

const ANTHROPIC_SIGNED: &str =
    "-R=anchor apple generic and certificate leaf[subject.OU] = \"Q6L2SF6YDW\"";
const CODESIGN: &str = "/usr/bin/codesign";
pub const CODESIGNED: Bundles = Bundles {
    is_anthropic: signed_by_anthropic,
    sign: ad_hoc_sign,
};
const IDENTIFIER: &str = "chat.condense.dense-engine";
const OLD: &str = "claude.dense-old.app";
const TMP: &str = "claude.dense-tmp.app";

/// The seal calls are fields so the filesystem logic runs without `codesign`.
#[derive(Clone, Copy)]
pub struct Bundles {
    is_anthropic: fn(&Path) -> bool,
    sign: fn(&Path) -> Result<()>,
}

pub fn executable(bundle: &Path) -> PathBuf {
    bundle.join("Contents").join("MacOS").join(BIN)
}

pub fn present(dir: &Path) -> bool {
    dir.join(BUNDLE).exists() || dir.join(REAL_BUNDLE).exists()
}

pub fn unwrap(dir: &Path) -> Result<bool> {
    let (app, real, old) = (dir.join(BUNDLE), dir.join(REAL_BUNDLE), dir.join(OLD));
    if !real.exists() {
        return Ok(false);
    }
    let _ = std::fs::remove_dir_all(&old);
    swap(&app, &real, &old)?;
    let _ = std::fs::remove_dir_all(&old);
    Ok(true)
}

/// A `claude.real.app` without a wrapper is an interrupted wrap: that repairs
/// here.
pub fn wrap(dir: &Path, exe: &Path, refresh: bool, bundles: Bundles) -> Result<Outcome> {
    let (app, real, old, tmp) = (
        dir.join(BUNDLE),
        dir.join(REAL_BUNDLE),
        dir.join(OLD),
        dir.join(TMP),
    );
    let _ = std::fs::remove_dir_all(&old);
    let ours = wrapped(dir);
    if ours && (!refresh || holds(&app, exe)?) {
        return Ok(Outcome::Current);
    }
    let genuine = app.exists() && !ours;
    // ponytail: a ~300MB hash; the watcher repeats it only when Desktop
    // changes the bundle or the retry interval passes.
    if genuine && !(bundles.is_anthropic)(&app) {
        return Ok(Outcome::Skipped(
            "claude.app isn't signed by Anthropic".to_string(),
        ));
    }

    let done = build(&tmp, exe, bundles).and_then(|()| {
        if !genuine {
            return swap(&app, &tmp, &old);
        }
        // A stale `claude.real.app` would block the move, and `app` just verified.
        let _ = std::fs::remove_dir_all(&real);
        swap(&app, &tmp, &real)
    });
    let _ = std::fs::remove_dir_all(&tmp);
    let _ = std::fs::remove_dir_all(&old);
    done.map(|()| Outcome::Wrapped)
}

/// Whether `claude.app` is our wrapper, not a genuine bundle beside a stale
/// `claude.real.app`. Only the plist, as the watcher asks every poll.
pub fn wrapped(dir: &Path) -> bool {
    dir.join(REAL_BUNDLE).exists()
        && std::fs::read_to_string(plist_path(&dir.join(BUNDLE)))
            .is_ok_and(|plist| plist.contains(IDENTIFIER))
}

fn ad_hoc_sign(wrapper: &Path) -> Result<()> {
    let out = Command::new(CODESIGN)
        .args(["--force", "--sign", "-", "--identifier", IDENTIFIER])
        .arg(wrapper)
        .output()
        .ctx("running codesign")?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::msg(format!(
        "codesign failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    )))
}

/// No symlinks in here: `codesign --verify --strict` rejects them.
fn build(tmp: &Path, exe: &Path, bundles: Bundles) -> Result<()> {
    let _ = std::fs::remove_dir_all(tmp);
    let bin = executable(tmp);
    if let Some(macos) = bin.parent() {
        std::fs::create_dir_all(macos).ctx(format!("creating {}", macos.display()))?;
    }
    let plist = plist_path(tmp);
    std::fs::write(&plist, info_plist(&file_sha256(exe)?))
        .ctx(format!("writing {}", plist.display()))?;
    place(exe, &bin)?;
    (bundles.sign)(tmp)
}

/// Whether `app` was built from `exe`. Signing rewrites the wrapped binary, so
/// only the hash recorded in the plist can say.
fn holds(app: &Path, exe: &Path) -> Result<bool> {
    let want = info_plist(&file_sha256(exe)?);
    Ok(std::fs::read_to_string(plist_path(app)).is_ok_and(|held| held == want))
}

fn info_plist(dense_sha256: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>CFBundleExecutable</key><string>{BIN}</string>\n\
         \x20 <key>CFBundleIdentifier</key><string>{IDENTIFIER}</string>\n\
         \x20 <key>DenseSha256</key><string>{dense_sha256}</string>\n\
         </dict>\n</plist>\n"
    )
}

fn plist_path(bundle: &Path) -> PathBuf {
    bundle.join("Contents").join("Info.plist")
}

fn signed_by_anthropic(app: &Path) -> bool {
    Command::new(CODESIGN)
        .args(["--verify", "--deep", "--strict", ANTHROPIC_SIGNED])
        .arg(app)
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{Install, install_at, installs};

    const DENSE: &[u8] = b"dense copy";
    const ENGINE: &[u8] = b"real engine";
    const FAKE_SEAL: Bundles = Bundles {
        is_anthropic: |_| true,
        sign: fake_sign,
    };

    /// Like codesign, leaves a signature and rewrites the binary.
    fn fake_sign(bundle: &Path) -> Result<()> {
        let bin = executable(bundle);
        let signed = [std::fs::read(&bin)?, b" signed".to_vec()].concat();
        std::fs::write(&bin, signed)?;
        std::fs::create_dir(bundle.join("Contents").join("_CodeSignature"))?;
        Ok(())
    }

    fn exe(root: &Path) -> PathBuf {
        let path = root.join("dense");
        std::fs::write(&path, DENSE).unwrap();
        path
    }

    fn published(root: &Path) -> Install {
        let dir = root.join("2.1.309").join("abcdef012345");
        let bin = executable(&dir.join(BUNDLE));
        std::fs::create_dir_all(bin.parent().unwrap()).unwrap();
        std::fs::write(&bin, ENGINE).unwrap();
        let plist = "<key>CFBundleIdentifier</key><string>com.anthropic.claude-code</string>";
        std::fs::write(plist_path(&dir.join(BUNDLE)), plist).unwrap();
        std::fs::write(dir.join(".verified"), "manifest-checksum").unwrap();
        install_at(&dir, "2.1.309")
    }

    fn tree(dir: &Path) -> Vec<String> {
        let mut found = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(tree(&path));
            } else {
                assert!(!path.is_symlink());
                found.push(path.display().to_string());
            }
        }
        found.sort();
        found
    }

    #[test]
    fn wrap_moves_the_original_whole_and_builds_a_clean_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());

        let outcome = found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();
        assert_eq!(outcome, Outcome::Wrapped);
        assert!(found.is_wrapped());
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
        assert_eq!(std::fs::read(&found.bin).unwrap(), b"dense copy signed");

        let wrapper = found.dir.join(BUNDLE);
        assert_eq!(
            tree(&wrapper),
            [plist_path(&wrapper), executable(&wrapper)].map(|p| p.display().to_string())
        );
        let plist = std::fs::read_to_string(plist_path(&wrapper)).unwrap();
        assert!(plist.contains(&format!("<string>{BIN}</string>")));
        assert!(plist.contains(IDENTIFIER));
        assert!(!found.dir.join(TMP).exists());
    }

    #[test]
    fn unwrap_restores_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();

        assert!(found.unwrap_engine().unwrap());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.dir.join(REAL_BUNDLE).exists());
        assert!(!found.is_wrapped());
        assert!(!found.unwrap_engine().unwrap());
    }

    #[test]
    fn interrupted_wrap_is_repaired() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        std::fs::rename(found.dir.join(BUNDLE), found.dir.join(REAL_BUNDLE)).unwrap();
        let found = installs(&[tmp.path().to_path_buf()]).remove(0);

        let outcome = found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();
        assert_eq!(outcome, Outcome::Wrapped);
        assert!(found.is_wrapped());
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn interrupted_unwrap_still_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();
        std::fs::rename(found.dir.join(BUNDLE), found.dir.join(OLD)).unwrap();

        assert!(found.unwrap_engine().unwrap());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.dir.join(OLD).exists());
    }

    #[test]
    fn refresh_replaces_and_resigns_only_a_changed_dense() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let exe = exe(tmp.path());
        found.wrap(&exe, false, FAKE_SEAL).unwrap();

        // Signing rewrote the wrapped copy; it still counts as this dense.
        assert_eq!(found.wrap(&exe, true, FAKE_SEAL).unwrap(), Outcome::Current);

        std::fs::write(&exe, b"newer dense").unwrap();
        assert_eq!(
            found.wrap(&exe, false, FAKE_SEAL).unwrap(),
            Outcome::Current
        );
        assert_eq!(found.wrap(&exe, true, FAKE_SEAL).unwrap(), Outcome::Wrapped);
        assert_eq!(std::fs::read(&found.bin).unwrap(), b"newer dense signed");
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
        assert!(!found.dir.join(OLD).exists());
    }

    #[test]
    fn a_bundle_anthropic_did_not_sign_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let bundles = Bundles {
            is_anthropic: |_| false,
            ..FAKE_SEAL
        };

        let outcome = found.wrap(&exe(tmp.path()), false, bundles).unwrap();
        assert!(matches!(outcome, Outcome::Skipped(why) if why.contains("Anthropic")));
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.dir.join(REAL_BUNDLE).exists());
    }

    #[test]
    fn a_genuine_bundle_beside_a_stale_real_counts_as_unwrapped() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let stale = executable(&found.dir.join(REAL_BUNDLE));
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"stale build").unwrap();
        assert!(!found.is_wrapped());

        let outcome = found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();
        assert_eq!(outcome, Outcome::Wrapped);
        assert!(found.is_wrapped());
        assert_eq!(std::fs::read(&found.real).unwrap(), ENGINE);
    }

    #[test]
    fn a_stale_real_never_replaces_a_bundle_anthropic_did_not_sign() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let stale = executable(&found.dir.join(REAL_BUNDLE));
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"stale build").unwrap();
        let bundles = Bundles {
            is_anthropic: |_| false,
            ..FAKE_SEAL
        };

        let outcome = found.wrap(&exe(tmp.path()), false, bundles).unwrap();
        assert!(matches!(outcome, Outcome::Skipped(_)));
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert_eq!(std::fs::read(&stale).unwrap(), b"stale build");
    }

    #[test]
    fn a_failed_signature_moves_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let bundles = Bundles {
            sign: |_| Err(Error::msg("no")),
            ..FAKE_SEAL
        };

        assert!(found.wrap(&exe(tmp.path()), false, bundles).is_err());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.dir.join(REAL_BUNDLE).exists());
        assert!(!found.dir.join(TMP).exists());
    }

    #[test]
    fn a_failed_swap_rolls_the_original_back() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        let bundles = Bundles {
            sign: |bundle| std::fs::remove_dir_all(bundle).map_err(Into::into),
            ..FAKE_SEAL
        };

        assert!(found.wrap(&exe(tmp.path()), false, bundles).is_err());
        assert_eq!(std::fs::read(&found.bin).unwrap(), ENGINE);
        assert!(!found.dir.join(REAL_BUNDLE).exists());
    }

    #[test]
    fn a_wrapped_bundle_is_found_by_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let found = published(tmp.path());
        found.wrap(&exe(tmp.path()), false, FAKE_SEAL).unwrap();

        let again = installs(&[tmp.path().to_path_buf()]);
        assert_eq!(again, [found]);
        assert!(again[0].is_wrapped());
    }
}
