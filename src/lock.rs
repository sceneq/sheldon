//! Plugin installation.
//!
//! This module handles the downloading of `Source`s and figuring out which
//! filenames to use for `Plugins`.

use std::{
    cmp,
    fmt,
    fs,
    io,
    path::{Path, PathBuf},
    result,
    sync,
};

use indexmap::IndexMap;
use itertools::Itertools;
use maplit::hashmap;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::{
    config::{Config, GitReference, Plugin, Source, Template},
    settings::Settings,
    Error,
    Result,
    ResultExt,
};

/// The default clone directory for `Git` sources.
const CLONE_DIRECTORY: &str = "repositories";

/// The default download directory for `Remote` sources.
const DOWNLOAD_DIRECTORY: &str = "downloads";

/// The maximmum number of threads to use while downloading sources.
const MAX_THREADS: u32 = 8;

/////////////////////////////////////////////////////////////////////////
// Locked configuration definitions
/////////////////////////////////////////////////////////////////////////

/// A locked `GitReference`.
#[derive(Clone, Debug)]
struct LockedGitReference(git::Oid);

/// A locked `Source`.
#[derive(Clone, Debug)]
struct LockedSource {
    /// The clone or download directory.
    directory: PathBuf,
    /// The download filename.
    filename: Option<PathBuf>,
}

/// A locked `Plugin`.
#[derive(Debug, Deserialize, Serialize)]
struct LockedPlugin {
    /// The name of this plugin.
    name: String,
    /// The directory that this plugin resides in.
    directory: PathBuf,
    /// The filenames to use in the directory.
    filenames: Vec<PathBuf>,
    /// What templates to apply to each filename.
    apply: Vec<String>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct LockedSettings {
    /// The current crate version.
    version: String,
    /// The location of the home directory.
    home: PathBuf,
    /// The location of the root directory.
    root: PathBuf,
    /// The location of the config file.
    config_file: PathBuf,
    /// The location of the lock file.
    lock_file: PathBuf,
}

/// A locked `Config`.
#[derive(Debug, Deserialize, Serialize)]
pub struct LockedConfig {
    /// The global settings that were used to generated this `LockedConfig`.
    #[serde(flatten)]
    pub settings: LockedSettings,
    /// A map of name to template.
    templates: IndexMap<String, Template>,
    /// Each locked plugin.
    plugins: Vec<LockedPlugin>,
}

/////////////////////////////////////////////////////////////////////////
// Lock implementations.
/////////////////////////////////////////////////////////////////////////

impl PartialEq<LockedSettings> for Settings {
    fn eq(&self, other: &LockedSettings) -> bool {
        self.version == other.version
            && self.home == other.home
            && self.root == other.root
            && self.config_file == other.config_file
            && self.lock_file == other.lock_file
    }
}

impl GitReference {
    /// Consume the `GitReference` and convert it to a `LockedGitReference`.
    ///
    /// This code is take from [Cargo].
    ///
    /// [Cargo]: https://github.com/rust-lang/cargo/blob/master/src/cargo/sources/git/utils.rs#L207
    fn lock(&self, repo: &git::Repository) -> Result<LockedGitReference> {
        let reference = match self {
            GitReference::Branch(s) => repo
                .find_branch(s, git::BranchType::Local)
                .ctx(s!("failed to find branch `{}`", s))?
                .get()
                .target()
                .ctx(s!("branch `{}` does not have a target", s))?,
            GitReference::Revision(s) => {
                let obj = repo
                    .revparse_single(s)
                    .ctx(s!("failed to find revision `{}`", s))?;
                match obj.as_tag() {
                    Some(tag) => tag.target_id(),
                    None => obj.id(),
                }
            }
            GitReference::Tag(s) => (|| -> result::Result<_, git::Error> {
                let id = repo.refname_to_id(&format!("refs/tags/{}", s))?;
                let obj = repo.find_object(id, None)?;
                let obj = obj.peel(git::ObjectType::Commit)?;
                Ok(obj.id())
            })()
            .ctx(s!("failed to find tag `{}`", s))?,
        };
        Ok(LockedGitReference(reference))
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Source::Git { url, .. } | Source::Remote { url } => write!(f, "{}", url),
            Source::Local { directory } => write!(f, "{}", directory.display()),
        }
    }
}

impl Source {
    /// Clone a Git repository and checks it out at a particular revision.
    fn lock_git(
        directory: PathBuf,
        url: Url,
        reference: Option<GitReference>,
    ) -> Result<LockedSource> {
        // Clone or open the repository.
        let repo = match git::Repository::clone(&url.to_string(), &directory) {
            Ok(repo) => repo,
            Err(e) => {
                if e.code() != git::ErrorCode::Exists {
                    return Err(e).ctx(s!("failed to git clone `{}`", url));
                } else {
                    git::Repository::open(&directory)
                        .ctx(s!("failed to open repository at `{}`", directory.display()))?
                }
            }
        };

        // Checkout the configured revision.
        if let Some(reference) = reference {
            let revision = reference.lock(&repo)?;

            let obj = repo
                .find_object(revision.0, None)
                .ctx(s!("failed to find revision `{}`", revision.0))?;
            repo.reset(&obj, git::ResetType::Hard, None).ctx(s!(
                "failed to reset repository to revision `{}`",
                revision.0
            ))?;
        }

        Ok(LockedSource {
            directory,
            filename: None,
        })
    }

    /// Downloads a Remote source.
    fn lock_remote(directory: PathBuf, filename: PathBuf, url: Url) -> Result<LockedSource> {
        if !filename.exists() {
            fs::create_dir_all(&directory)
                .ctx(s!("failed to create directory `{}`", directory.display()))?;
            let mut response =
                request::get(url.clone()).ctx(s!("failed to download from `{}`", url))?;
            let mut out =
                fs::File::create(&filename).ctx(s!("failed to create `{}`", filename.display()))?;
            io::copy(&mut response, &mut out)
                .ctx(s!("failed to copy contents to `{}`", filename.display()))?;
        }
        Ok(LockedSource {
            directory,
            filename: Some(filename),
        })
    }

    /// Checks that a Local source directory exists.
    fn lock_local(settings: &Settings, directory: PathBuf) -> Result<LockedSource> {
        let directory = settings.expand_tilde(directory);

        if fs::metadata(&directory)
            .ctx(s!("failed to find directory `{}`", directory.display()))?
            .is_dir()
        {
            Ok(LockedSource {
                directory,
                filename: None,
            })
        } else {
            bail!("`{}` is not a directory", directory.display());
        }
    }

    /// Install this `Source`.
    fn lock(self, settings: &Settings) -> Result<LockedSource> {
        match self {
            Source::Git { url, reference } => {
                let mut directory = settings.root.join(CLONE_DIRECTORY);
                directory.push(url.host_str().ctx(s!("URL `{}` has no host", url))?);
                directory.push(url.path().trim_start_matches('/'));
                Self::lock_git(directory, url, reference)
            }
            Source::Remote { url } => {
                let mut directory = settings.root.join(DOWNLOAD_DIRECTORY);
                directory.push(url.host_str().ctx(s!("URL `{}` has no host", url))?);

                let segments: Vec<_> = url
                    .path_segments()
                    .ctx(s!("URL `{}` is cannot-be-a-base", url))?
                    .collect();
                let (base, rest) = segments.split_last().unwrap();
                let base = if *base != "" { *base } else { "index" };
                directory.push(rest.iter().collect::<PathBuf>());
                let filename = directory.join(base);

                Self::lock_remote(directory, filename, url)
            }
            Source::Local { directory } => Self::lock_local(settings, directory),
        }
    }
}

impl Plugin {
    fn match_globs(pattern: PathBuf, filenames: &mut Vec<PathBuf>) -> Result<bool> {
        let mut matched = false;
        let pattern = pattern.to_string_lossy();
        let paths: glob::Paths =
            glob::glob(&pattern).ctx(s!("failed to parse glob pattern `{}`", &pattern))?;

        for path in paths {
            filenames.push(path.ctx(s!("failed to read path matched by pattern `{}`", &pattern))?);
            matched = true;
        }

        Ok(matched)
    }

    /// Consume the `Plugin` and convert it to a `LockedPlugin`.
    fn lock(
        self,
        settings: &Settings,
        source: LockedSource,
        matches: &[String],
        apply: &[String],
    ) -> Result<LockedPlugin> {
        Ok(if let Source::Remote { .. } = self.source {
            let LockedSource {
                directory,
                filename,
            } = source;
            LockedPlugin {
                name: self.name,
                directory,
                filenames: vec![filename.unwrap()],
                apply: self.apply.unwrap_or_else(|| apply.to_vec()),
            }
        } else {
            let LockedSource { directory, .. } = source;
            let mut filenames = Vec::new();
            let mut templates = handlebars::Handlebars::new();
            templates.set_strict_mode(true);

            // Data to use in template rendering
            let data = hashmap! {
                "root" => settings
                    .root
                    .to_str()
                    .ctx(s!("root directory is not valid UTF-8"))?,
                "name" => &self.name,
                "directory" => &directory
                    .to_str()
                    .ctx(s!("root directory is not valid UTF-8"))?,
            };

            // If the plugin defined what files to use, we do all of them.
            if let Some(uses) = &self.uses {
                for u in uses {
                    let rendered = templates
                        .render_template(u, &data)
                        .ctx(s!("failed to render template `{}`", u))?;
                    let pattern = directory.join(&rendered);
                    if !Self::match_globs(pattern, &mut filenames)? {
                        bail!("failed to find any files matching `{}`", &rendered);
                    };
                }
            // Otherwise we try to figure out which files to use...
            } else {
                for g in matches {
                    let rendered = templates
                        .render_template(g, &data)
                        .ctx(s!("failed to render template `{}`", g))?;
                    let pattern = directory.join(rendered);
                    if Self::match_globs(pattern, &mut filenames)? {
                        break;
                    }
                }
            }

            LockedPlugin {
                name: self.name,
                directory,
                filenames,
                apply: self.apply.unwrap_or_else(|| apply.to_vec()),
            }
        })
    }
}

impl Settings {
    /// Consume the `Settings` and convert it to a `LockedSettings`.
    fn lock(self) -> LockedSettings {
        LockedSettings {
            version: self.version.to_string(),
            home: self.home,
            root: self.root,
            config_file: self.config_file,
            lock_file: self.lock_file,
        }
    }
}

impl Config {
    /// Consume the `Config` and convert it to a `LockedConfig`.
    ///
    /// This method installs all necessary remote dependencies of plugins,
    /// validates that local plugins are present, and checks that templates
    /// can compile.
    pub fn lock(self, settings: &Settings) -> Result<LockedConfig> {
        // Create a map of unique `Source` to `Vec<Plugin>`
        let mut map = IndexMap::new();
        for (index, plugin) in self.plugins.into_iter().enumerate() {
            map.entry(plugin.source.clone())
                .or_insert_with(|| Vec::with_capacity(1))
                .push((index, plugin));
        }

        let matches = &self.matches;
        let apply = &self.apply;
        let count = map.len();

        let plugins = if count == 0 {
            Vec::new()
        } else {
            // Create a thread pool and install the sources in parallel.
            let mut pool = scoped_threadpool::Pool::new(cmp::min(count as u32, MAX_THREADS));
            let (tx, rx) = sync::mpsc::channel();

            pool.scoped(|scoped| {
                for (source, plugins) in map {
                    let tx = tx.clone();
                    scoped.execute(move || {
                        tx.send((|| {
                            let source_name = format!("{}", source);
                            let source = source
                                .lock(settings)
                                .ctx(s!("failed to install source `{}`", source_name))?;
                            let mut locked = Vec::with_capacity(plugins.len());

                            for (index, plugin) in plugins {
                                let name = plugin.name.clone();

                                locked.push((
                                    index,
                                    plugin
                                        .lock(settings, source.clone(), matches, apply)
                                        .ctx(s!("failed to install plugin `{}`", name))?,
                                ));
                            }
                            Ok(locked)
                        })())
                        .expect("oops! did main thread die?");
                    })
                }
                scoped.join_all();
            });

            rx.iter()
                .take(count)
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .into_iter()
                .sorted_by_key(|(index, _)| *index)
                .map(|(_, plugin)| plugin)
                .collect()
        };

        Ok(LockedConfig {
            settings: settings.clone().lock(),
            templates: self.templates,
            plugins,
        })
    }
}

impl LockedConfig {
    /// Read a `LockedConfig` from the given path.
    pub fn from_path<P>(path: P) -> Result<Self>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();;
        let locked: LockedConfig = toml::from_str(&String::from_utf8_lossy(
            &fs::read(&path).ctx(s!("failed to read locked config from `{}`", path.display()))?,
        ))
        .ctx(s!("failed to deserialize locked config"))?;
        Ok(locked)
    }

    /// Generate the script.
    pub fn source(&self) -> Result<String> {
        // Compile the templates
        let mut templates = handlebars::Handlebars::new();
        templates.set_strict_mode(true);
        for (name, template) in &self.templates {
            templates
                .register_template_string(&name, &template.value)
                .ctx(s!("failed to compile template `{}`", name))?;
        }

        let mut script = String::new();

        for plugin in &self.plugins {
            for name in &plugin.apply {
                // Data to use in template rendering
                let mut data = hashmap! {
                    "root" => self
                        .settings.root
                        .to_str()
                        .ctx(s!("root directory is not valid UTF-8"))?,
                    "name" => &plugin.name,
                    "directory" => plugin
                        .directory
                        .to_str()
                        .ctx(s!("root directory is not valid UTF-8"))?,
                };

                if self.templates[name].each {
                    for filename in &plugin.filenames {
                        data.insert(
                            "filename",
                            filename.to_str().ctx(s!("filename is not valid UTF-8"))?,
                        );
                        script.push_str(
                            &templates
                                .render(name, &data)
                                .ctx(s!("failed to render template `{}`", name))?,
                        );
                        script.push('\n');
                    }
                } else {
                    script.push_str(
                        &templates
                            .render(name, &data)
                            .ctx(s!("failed to render template `{}`", name))?,
                    );
                    script.push('\n');
                }
            }
        }

        Ok(script)
    }

    /// Write a `LockedConfig` config to the given path.
    pub fn to_path<P>(&self, path: P) -> Result<()>
    where
        P: AsRef<Path>,
    {
        let path = path.as_ref();

        fs::write(
            path,
            &toml::to_string(&self).ctx(s!("failed to serialize locked config"))?,
        )
        .ctx(s!("failed to write locked config to `{}`", path.display()))?;

        Ok(())
    }
}

/////////////////////////////////////////////////////////////////////////
// Unit tests
/////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read, process::Command};
    use url::Url;

    fn git_create_test_repo(directory: &Path) {
        Command::new("git")
            .arg("-C")
            .arg(&directory)
            .arg("init")
            .output()
            .unwrap();
        Command::new("touch")
            .arg(directory.join("test.txt"))
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("add")
            .arg(".")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("commit")
            .arg("-m")
            .arg("Initial commit")
            .output()
            .unwrap();
        Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("tag")
            .arg("derp")
            .output()
            .unwrap();
    }

    fn git_get_last_commit(directory: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("log")
            .arg("-n")
            .arg("1")
            .arg("--pretty=format:\"%H\"")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout.trim().trim_matches('"').to_string()
    }

    fn git_status(directory: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(directory)
            .arg("status")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout).to_string()
    }

    fn read_file_contents(filename: &Path) -> result::Result<String, io::Error> {
        let mut file = fs::File::open(filename)?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)?;
        Ok(contents)
    }

    #[test]
    fn git_reference_lock_tag() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git_create_test_repo(&directory);
        let hash = git_get_last_commit(&directory);
        let repo = git::Repository::open(directory).unwrap();

        let reference = GitReference::Tag("derp".to_string());
        let locked = reference.lock(&repo).unwrap();

        assert_eq!(locked.0.to_string(), hash);
    }

    #[test]
    fn git_reference_lock_branch() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git_create_test_repo(&directory);
        let hash = git_get_last_commit(&directory);
        let repo = git::Repository::open(directory).unwrap();

        let reference = GitReference::Branch("master".to_string());
        let locked = reference.lock(&repo).unwrap();

        assert_eq!(locked.0.to_string(), hash);
    }

    #[test]
    fn git_reference_lock_revision() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        git_create_test_repo(&directory);
        let hash = git_get_last_commit(&directory);
        let repo = git::Repository::open(directory).unwrap();

        let reference = GitReference::Revision(hash.clone());
        let locked = reference.lock(&repo).unwrap();

        assert_eq!(locked.0.to_string(), hash);
    }

    #[test]
    fn source_lock_git() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();

        let locked = Source::lock_git(
            directory.to_path_buf(),
            Url::parse("https://github.com/rossmacarthur/sheldon").unwrap(),
            None,
        )
        .unwrap();

        assert_eq!(locked.directory, directory);
        assert_eq!(locked.filename, None);
        assert_eq!(
            git_status(&directory),
            "On branch master\nYour branch is up to date with 'origin/master'.\n\nnothing to \
             commit, working tree clean\n"
        );
    }

    #[test]
    fn source_lock_git_with_reference() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();

        let locked = Source::lock_git(
            directory.to_path_buf(),
            Url::parse("https://github.com/rossmacarthur/sheldon").unwrap(),
            Some(GitReference::Tag("0.2.0".to_string())),
        )
        .unwrap();

        assert_eq!(locked.directory, directory);
        assert_eq!(locked.filename, None);
        assert_eq!(
            git_get_last_commit(&directory),
            "a2cf341b37c958e490aafc92dd775c597addf3c4"
        );
    }

    #[test]
    fn source_lock_remote() {
        let manifest_dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path();
        let filename = directory.join("test.txt");

        let locked = Source::lock_remote(
            directory.to_path_buf(),
            filename.clone(),
            Url::parse("https://github.com/rossmacarthur/sheldon/raw/0.3.0/LICENSE-MIT").unwrap(),
        )
        .unwrap();

        assert_eq!(locked.directory, directory);
        assert_eq!(locked.filename, Some(filename.clone()));
        assert_eq!(
            read_file_contents(&filename).unwrap(),
            read_file_contents(&manifest_dir.join("LICENSE-MIT")).unwrap()
        )
    }

    fn create_test_settings(root: &str) -> Settings {
        let root = PathBuf::from(root);
        Settings {
            version: clap::crate_version!(),
            home: "/".into(),
            config_file: root.join("config.toml"),
            lock_file: root.join("config.lock"),
            root,
            reinstall: false,
            relock: false,
        }
    }

    #[test]
    fn plugin_lock_git_with_uses() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directory = root.join("repositories/github.com/rossmacarthur/sheldon");
        fs::create_dir_all(&directory).unwrap();
        fs::File::create(directory.join("1.txt")).unwrap();
        fs::File::create(directory.join("2.txt")).unwrap();
        fs::File::create(directory.join("test.html")).unwrap();

        let plugin = Plugin {
            name: "test".into(),
            source: Source::Git {
                url: Url::parse("https://github.com/rossmacarthur/sheldon").unwrap(),
                reference: None,
            },
            uses: Some(vec!["*.txt".into(), "{{ name }}.html".into()]),
            apply: None,
        };
        let locked = plugin
            .lock(
                &create_test_settings(&root.to_string_lossy()),
                LockedSource {
                    directory: directory.clone(),
                    filename: None,
                },
                &[],
                &["hello".into()],
            )
            .unwrap();

        assert_eq!(locked.name, String::from("test"));
        assert_eq!(locked.directory, directory);
        assert_eq!(
            locked.filenames,
            vec![
                directory.join("1.txt"),
                directory.join("2.txt"),
                directory.join("test.html")
            ]
        );
        assert_eq!(locked.apply, vec![String::from("hello")]);
    }

    #[test]
    fn plugin_lock_git_with_matches() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let directory = root.join("repositories/github.com/rossmacarthur/sheldon");
        fs::create_dir_all(&directory).unwrap();
        fs::File::create(directory.join("1.txt")).unwrap();
        fs::File::create(directory.join("2.txt")).unwrap();
        fs::File::create(directory.join("test.html")).unwrap();

        let plugin = Plugin {
            name: "test".into(),
            source: Source::Git {
                url: Url::parse("https://github.com/rossmacarthur/sheldon").unwrap(),
                reference: None,
            },
            uses: None,
            apply: None,
        };
        let locked = plugin
            .lock(
                &create_test_settings(&root.to_string_lossy()),
                LockedSource {
                    directory: directory.clone(),
                    filename: None,
                },
                &["*.txt".into(), "test.html".into()],
                &["hello".into()],
            )
            .unwrap();

        assert_eq!(locked.name, String::from("test"));
        assert_eq!(locked.directory, directory);
        assert_eq!(
            locked.filenames,
            vec![directory.join("1.txt"), directory.join("2.txt")]
        );
        assert_eq!(locked.apply, vec![String::from("hello")]);
    }

    #[test]
    fn plugin_lock_remote() {
        let plugin = Plugin {
            name: "test".into(),
            source: Source::Remote {
                url: Url::parse("https://ross.macarthur.io/test.html").unwrap(),
            },
            uses: None,
            apply: None,
        };
        let locked = plugin
            .lock(
                &create_test_settings("/home/test"),
                LockedSource {
                    directory: "/home/test/downloads/ross.macarthur.io".into(),
                    filename: Some("/home/test/downloads/ross.macarthur.io/test.html".into()),
                },
                &[],
                &["hello".into()],
            )
            .unwrap();

        assert_eq!(locked.name, String::from("test"));
        assert_eq!(
            locked.directory,
            PathBuf::from("/home/test/downloads/ross.macarthur.io")
        );
        assert_eq!(
            locked.filenames,
            vec![PathBuf::from(
                "/home/test/downloads/ross.macarthur.io/test.html"
            )]
        );
        assert_eq!(locked.apply, vec![String::from("hello")]);
    }

    #[test]
    fn config_lock_example_config() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let settings = Settings {
            version: clap::crate_version!(),
            home: "/".into(),
            root: root.to_path_buf(),
            config_file: manifest_dir.join("docs/plugins.example.toml"),
            lock_file: root.join("plugins.lock"),
            reinstall: false,
            relock: false,
        };
        let pyenv_dir = root.join("pyenv");
        fs::create_dir(&pyenv_dir).unwrap();

        let mut config = Config::from_path(&settings.config_file).unwrap();
        config.plugins[2].source = Source::Local {
            directory: pyenv_dir,
        };
        config.lock(&settings).unwrap();
    }
}
