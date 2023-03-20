use super::activity;
use crate::{fs, projects, users};
use anyhow::{anyhow, Context, Result};
use filetime::FileTime;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufReader, Read},
    os::unix::prelude::MetadataExt,
    path::Path,
    time,
};
use uuid::Uuid;

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Meta {
    // timestamp of when the session was created
    pub start_timestamp_ms: u128,
    // timestamp of when the session was last active
    pub last_timestamp_ms: u128,
    // session branch name
    pub branch: Option<String>,
    // session commit hash
    pub commit: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    // if hash is not set, the session is not saved aka current
    pub hash: Option<String>,
    pub meta: Meta,
    pub activity: Vec<activity::Activity>,
}

impl Session {
    pub fn from_head(repo: &git2::Repository, project: &projects::Project) -> Result<Self> {
        let now_ts = time::SystemTime::now()
            .duration_since(time::UNIX_EPOCH)
            .unwrap()
            .as_millis();

        let activity = match std::fs::read_to_string(repo.path().join("logs/HEAD")) {
            Ok(reflog) => reflog
                .lines()
                .filter_map(|line| activity::parse_reflog_line(line).ok())
                .filter(|activity| activity.timestamp_ms >= now_ts)
                .collect::<Vec<activity::Activity>>(),
            Err(_) => Vec::new(),
        };

        let meta = match repo.head() {
            Ok(head) => Meta {
                start_timestamp_ms: now_ts,
                last_timestamp_ms: now_ts,
                branch: Some(head.name().unwrap().to_string()),
                commit: Some(head.peel_to_commit().unwrap().id().to_string()),
            },
            Err(_) => Meta {
                start_timestamp_ms: now_ts,
                last_timestamp_ms: now_ts,
                branch: None,
                commit: None,
            },
        };

        let session = Session {
            id: Uuid::new_v4().to_string(),
            hash: None,
            meta,
            activity,
        };
        create(project, &session).with_context(|| "failed to create current session from head")?;
        Ok(session)
    }

    pub fn from_commit(repo: &git2::Repository, commit: &git2::Commit) -> Result<Self> {
        let tree = commit.tree().with_context(|| {
            format!("failed to get tree from commit {}", commit.id().to_string())
        })?;

        let start_timestamp_ms = read_as_string(repo, &tree, Path::new("session/meta/start"))?
            .parse::<u128>()
            .with_context(|| {
                format!(
                    "failed to parse start timestamp from commit {}",
                    commit.id().to_string()
                )
            })?;

        let logs_path = Path::new("logs/HEAD");
        let activity = match tree.get_path(logs_path).is_ok() {
            true => read_as_string(repo, &tree, logs_path)
                .with_context(|| {
                    format!(
                        "failed to read reflog from commit {}",
                        commit.id().to_string()
                    )
                })?
                .lines()
                .filter_map(|line| activity::parse_reflog_line(line).ok())
                .filter(|activity| activity.timestamp_ms >= start_timestamp_ms)
                .collect::<Vec<activity::Activity>>(),
            false => Vec::new(),
        };

        let branch_path = Path::new("session/meta/branch");
        let session_branch = match tree.get_path(branch_path).is_ok() {
            true => read_as_string(repo, &tree, branch_path)
                .with_context(|| {
                    format!(
                        "failed to read branch name from commit {}",
                        commit.id().to_string()
                    )
                })?
                .into(),
            false => None,
        };

        let commit_path = Path::new("session/meta/commit");
        let session_commit = match tree.get_path(commit_path).is_ok() {
            true => read_as_string(repo, &tree, commit_path)
                .with_context(|| {
                    format!(
                        "failed to read branch name from commit {}",
                        commit.id().to_string()
                    )
                })?
                .into(),
            false => None,
        };

        Ok(Session {
            id: read_as_string(repo, &tree, Path::new("session/meta/id")).with_context(|| {
                format!(
                    "failed to read session id from commit {}",
                    commit.id().to_string()
                )
            })?,
            hash: Some(commit.id().to_string()),
            meta: Meta {
                start_timestamp_ms,
                last_timestamp_ms: read_as_string(repo, &tree, Path::new("session/meta/last"))?
                    .parse::<u128>()
                    .with_context(|| {
                        format!(
                            "failed to parse last timestamp from commit {}",
                            commit.id().to_string()
                        )
                    })?,
                branch: session_branch,
                commit: session_commit,
            },
            activity,
        })
    }

    pub fn touch(&mut self, project: &projects::Project) -> Result<()> {
        update(project, self)
    }

    pub fn flush(
        &mut self,
        repo: &git2::Repository,
        user: &Option<users::User>,
        project: &projects::Project,
    ) -> Result<()> {
        flush(repo, user, project, self)
    }
}

fn write(session_path: &Path, session: &Session) -> Result<()> {
    if session.hash.is_some() {
        return Err(anyhow!("cannot write session that is not current"));
    }

    let meta_path = session_path.join("meta");

    std::fs::create_dir_all(meta_path.clone()).with_context(|| {
        format!(
            "failed to create session meta directory {}",
            meta_path.display()
        )
    })?;

    let id_path = meta_path.join("id");
    std::fs::write(id_path.clone(), session.id.clone())
        .with_context(|| format!("failed to write session id to {}", id_path.display()))?;

    let start_path = meta_path.join("start");
    std::fs::write(
        start_path.clone(),
        session.meta.start_timestamp_ms.to_string(),
    )
    .with_context(|| {
        format!(
            "failed to write session start timestamp to {}",
            start_path.display()
        )
    })?;

    let last_path = meta_path.join("last");
    std::fs::write(
        last_path.clone(),
        session.meta.last_timestamp_ms.to_string(),
    )
    .with_context(|| {
        format!(
            "failed to write session last timestamp to {}",
            last_path.display()
        )
    })?;

    if let Some(branch) = session.meta.branch.clone() {
        let branch_path = meta_path.join("branch");
        std::fs::write(branch_path.clone(), branch).with_context(|| {
            format!(
                "failed to write session branch to {}",
                branch_path.display()
            )
        })?;
    }

    if let Some(commit) = session.meta.commit.clone() {
        let commit_path = meta_path.join("commit");
        std::fs::write(commit_path.clone(), commit).with_context(|| {
            format!(
                "failed to write session commit to {}",
                commit_path.display()
            )
        })?;
    }

    Ok(())
}

fn update(project: &projects::Project, session: &mut Session) -> Result<()> {
    session.meta.last_timestamp_ms = time::SystemTime::now()
        .duration_since(time::UNIX_EPOCH)
        .unwrap()
        .as_millis();

    let session_path = project.session_path();
    log::debug!("{}: updating current session", project.id);
    if session_path.exists() {
        write(&session_path, session)
    } else {
        Err(anyhow!("\"{}\" does not exist", session_path.display()))
    }
}

fn create(project: &projects::Project, session: &Session) -> Result<()> {
    let session_path = project.session_path();
    log::debug!("{}: Creating current session", session_path.display());
    let meta_path = session_path.join("meta");
    if meta_path.exists() {
        Err(anyhow!("session already exists"))
    } else {
        write(&session_path, session)
    }
}

fn delete(project: &projects::Project) -> Result<()> {
    let session_path = project.session_path();
    log::debug!("{}: deleting current session", project.id);
    if session_path.exists() {
        std::fs::remove_dir_all(session_path)?;
    }
    Ok(())
}

pub fn id_from_commit(repo: &git2::Repository, commit: &git2::Commit) -> Result<String> {
    let tree = commit.tree().unwrap();
    let session_id_path = Path::new("session/meta/id");
    if !tree.get_path(session_id_path).is_ok() {
        return Err(anyhow!("commit does not have a session id"));
    }
    let id = read_as_string(repo, &tree, session_id_path)?;
    return Ok(id);
}

fn read_as_string(repo: &git2::Repository, tree: &git2::Tree, path: &Path) -> Result<String> {
    let tree_entry = tree.get_path(path)?;
    let blob = tree_entry.to_object(repo)?.into_blob().unwrap();
    let contents = String::from_utf8(blob.content().to_vec()).with_context(|| {
        format!(
            "failed to read file {} as string",
            path.to_str().unwrap_or("unknown")
        )
    })?;
    Ok(contents)
}

fn flush(
    repo: &git2::Repository,
    user: &Option<users::User>,
    project: &projects::Project,
    session: &mut Session,
) -> Result<()> {
    if session.hash.is_some() {
        return Err(anyhow!(
            "refuse to flush {} because it already has a hash",
            session.id
        ));
    }

    session
        .touch(project)
        .with_context(|| format!("failed to touch session"))?;

    let wd_tree = build_wd_tree(&repo, &project)
        .with_context(|| "failed to build wd tree for project".to_string())?;

    let session_index =
        &mut git2::Index::new().with_context(|| format!("failed to create session index"))?;
    build_session_index(&repo, project, session_index)
        .with_context(|| format!("failed to build session index"))?;
    let session_tree = session_index
        .write_tree_to(&repo)
        .with_context(|| format!("failed to write session tree"))?;

    let log_index =
        &mut git2::Index::new().with_context(|| format!("failed to create log index"))?;
    build_log_index(&repo, log_index).with_context(|| format!("failed to build log index"))?;
    let log_tree = log_index
        .write_tree_to(&repo)
        .with_context(|| format!("failed to write log tree"))?;

    let mut tree_builder = repo
        .treebuilder(None)
        .with_context(|| format!("failed to create tree builder"))?;
    tree_builder
        .insert("session", session_tree, 0o040000)
        .with_context(|| format!("failed to insert session tree"))?;
    tree_builder
        .insert("wd", wd_tree, 0o040000)
        .with_context(|| format!("failed to insert wd tree"))?;
    tree_builder
        .insert("logs", log_tree, 0o040000)
        .with_context(|| format!("failed to insert log tree"))?;

    let tree = tree_builder
        .write()
        .with_context(|| format!("failed to write tree"))?;

    let commit_oid = write_gb_commit(tree, &repo, user, project).with_context(|| {
        format!(
            "failed to write gb commit for {}",
            repo.workdir().unwrap().display()
        )
    })?;

    log::info!(
        "{}: flushed session {} into commit {}",
        project.id,
        session.id,
        commit_oid,
    );

    session.hash = Some(commit_oid.to_string());

    delete(project)?;

    if let Err(e) = push_to_remote(repo, user, project) {
        log::error!(
            "{}: failed to push gb commit {} to remote: {:#}",
            project.id,
            commit_oid,
            e
        );
    }

    Ok(())
}

// try to push the new gb history head to the remote
// TODO: if we see it is not a FF, pull down the remote, determine order, rewrite the commit line, and push again
fn push_to_remote(
    repo: &git2::Repository,
    user: &Option<users::User>,
    project: &projects::Project,
) -> Result<()> {
    // only push if logged in
    let access_token = match user {
        Some(user) => user.access_token.clone(),
        None => return Ok(()),
    };

    // only push if project is connected
    let remote_url = match project.api {
        Some(ref api) => api.git_url.clone(),
        None => return Ok(()),
    };

    log::info!("pushing {} to {}", project.path, remote_url);

    // Create an anonymous remote
    let mut remote = repo
        .remote_anonymous(remote_url.as_str())
        .with_context(|| {
            format!(
                "failed to create anonymous remote for {}",
                remote_url.as_str()
            )
        })?;

    // Set the remote's callbacks
    let mut callbacks = git2::RemoteCallbacks::new();
    callbacks.push_update_reference(move |refname, message| {
        log::info!(
            "{}: pushing reference '{}': {:?}",
            project.path,
            refname,
            message
        );
        Ok(())
    });
    callbacks.push_transfer_progress(move |one, two, three| {
        log::info!(
            "{}: transferred {}/{}/{} objects",
            project.path,
            one,
            two,
            three
        );
    });

    let mut push_options = git2::PushOptions::new();
    push_options.remote_callbacks(callbacks);
    let auth_header = format!("Authorization: {}", access_token);
    let headers = &[auth_header.as_str()];
    push_options.custom_headers(headers);

    // Push to the remote
    remote
        .push(&[project.refname()], Some(&mut push_options))
        .with_context(|| {
            format!(
                "failed to push {} to {}",
                project.refname(),
                remote_url.as_str()
            )
        })?;

    Ok(())
}

fn build_wd_tree(repo: &git2::Repository, project: &projects::Project) -> Result<git2::Oid> {
    let wd_index = &mut git2::Index::new()
        .with_context(|| format!("failed to create index for working directory"))?;
    match repo.find_reference(&project.refname()) {
        Ok(reference) => {
            // build the working directory tree from the current commit
            // and the session files
            let commit = reference.peel_to_commit()?;
            let tree = commit.tree()?;
            let wd_tree_entry = tree.get_name("wd").unwrap();
            let wd_tree = repo.find_tree(wd_tree_entry.id())?;
            wd_index.read_tree(&wd_tree)?;

            let session_wd_files = fs::list_files(project.wd_path()).with_context(|| {
                format!("failed to list files in {}", project.wd_path().display())
            })?;
            for file in session_wd_files {
                let abs_path = project.wd_path().join(&file);
                let metadata = abs_path
                    .metadata()
                    .with_context(|| "failed to get metadata for".to_string())?;
                let mtime = FileTime::from_last_modification_time(&metadata);
                let ctime = FileTime::from_creation_time(&metadata).unwrap_or(mtime);
                let file_content = std::fs::read_to_string(&abs_path)
                    .with_context(|| format!("failed to read file {}", abs_path.display()))?;
                wd_index
                    .add(&git2::IndexEntry {
                        ctime: git2::IndexTime::new(
                            ctime.seconds().try_into()?,
                            ctime.nanoseconds().try_into().unwrap(),
                        ),
                        mtime: git2::IndexTime::new(
                            mtime.seconds().try_into()?,
                            mtime.nanoseconds().try_into().unwrap(),
                        ),
                        dev: metadata.dev().try_into()?,
                        ino: metadata.ino().try_into()?,
                        mode: 33188,
                        uid: metadata.uid().try_into().unwrap(),
                        gid: metadata.gid().try_into().unwrap(),
                        file_size: metadata.len().try_into().unwrap(),
                        flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
                        flags_extended: 0, // no extended flags
                        path: file.to_str().unwrap().to_string().into(),
                        id: repo.blob(file_content.as_bytes())?,
                    })
                    .with_context(|| format!("failed to add index entry for {}", file.display()))?;
            }

            let wd_tree_oid = wd_index
                .write_tree_to(&repo)
                .with_context(|| format!("failed to write wd tree"))?;
            Ok(wd_tree_oid)
        }
        Err(e) => {
            if e.code() != git2::ErrorCode::NotFound {
                return Err(e.into());
            }
            build_wd_index_from_repo(&repo, &project, wd_index)
                .with_context(|| format!("failed to build wd index"))?;
            let wd_tree_oid = wd_index
                .write_tree_to(&repo)
                .with_context(|| format!("failed to write wd tree"))?;
            Ok(wd_tree_oid)
        }
    }
}

// build wd index from the working directory files new session wd files
// this is important because we want to make sure session files are in sync with session deltas
fn build_wd_index_from_repo(
    repo: &git2::Repository,
    project: &projects::Project,
    index: &mut git2::Index,
) -> Result<()> {
    // create a new in-memory git2 index and open the working one so we can cheat if none of the metadata of an entry has changed
    let repo_index = &mut repo
        .index()
        .with_context(|| format!("failed to open repo index"))?;

    let mut added: HashMap<String, bool> = HashMap::new();

    // first, add session/wd files. this is important because we want to make sure session files
    // are in sync with session deltas
    let session_wd_files = fs::list_files(project.wd_path())
        .with_context(|| format!("failed to list files in {}", project.wd_path().display()))?;
    for file in session_wd_files {
        let file_path = Path::new(&file);
        if repo.is_path_ignored(&file).unwrap_or(true) {
            continue;
        }
        add_wd_path(index, repo_index, &project.wd_path(), &file_path, &repo).with_context(
            || {
                format!(
                    "failed to add working directory path {}",
                    file_path.display()
                )
            },
        )?;
        added.insert(file_path.to_string_lossy().to_string(), true);
    }

    // finally, add files from the working directory if they aren't already in the index
    let current_wd_files = fs::list_files(&project.path)
        .with_context(|| format!("failed to list files in {}", project.path))?;

    for file in current_wd_files {
        if added.contains_key(&file.to_string_lossy().to_string()) {
            continue;
        }

        let file_path = Path::new(&file);
        if !repo.is_path_ignored(&file).unwrap_or(true) {
            add_wd_path(
                index,
                repo_index,
                &Path::new(&project.path),
                &file_path,
                &repo,
            )
            .with_context(|| {
                format!(
                    "failed to add working directory path {}",
                    file_path.display()
                )
            })?;
        }
    }

    Ok(())
}

// take a file path we see and add it to our in-memory index
// we call this from build_initial_wd_tree, which is smart about using the existing index to avoid rehashing files that haven't changed
// and also looks for large files and puts in a placeholder hash in the LFS format
// TODO: actually upload the file to LFS
fn add_wd_path(
    index: &mut git2::Index,
    repo_index: &mut git2::Index,
    dir: &Path,
    rel_file_path: &Path,
    repo: &git2::Repository,
) -> Result<()> {
    let abs_file_path = dir.join(rel_file_path);
    let file_path = Path::new(&abs_file_path);

    let metadata = file_path
        .metadata()
        .with_context(|| "failed to get metadata for".to_string())?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap_or(mtime);

    if let Some(entry) = repo_index.get_path(rel_file_path, 0) {
        // if we find the entry and the metadata of the file has not changed, we can just use the existing entry
        if entry.mtime.seconds() == i32::try_from(mtime.seconds())?
            && entry.mtime.nanoseconds() == u32::try_from(mtime.nanoseconds()).unwrap()
            && entry.file_size == u32::try_from(metadata.len())?
            && entry.mode == metadata.mode()
        {
            log::debug!("using existing entry for {}", file_path.display());
            index.add(&entry).unwrap();
            return Ok(());
        }
    }

    // something is different, or not found, so we need to create a new entry

    log::debug!("adding wd path: {}", file_path.display());

    // look for files that are bigger than 4GB, which are not supported by git
    // insert a pointer as the blob content instead
    // TODO: size limit should be configurable
    let blob = if metadata.len() > 100_000_000 {
        log::debug!(
            "{}: file too big: {}",
            repo.workdir().unwrap().display(),
            file_path.display()
        );

        // get a sha256 hash of the file first
        let sha = sha256_digest(&file_path)?;

        // put togther a git lfs pointer file: https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md
        let mut lfs_pointer = String::from("version https://git-lfs.github.com/spec/v1\n");
        lfs_pointer.push_str("oid sha256:");
        lfs_pointer.push_str(&sha);
        lfs_pointer.push_str("\n");
        lfs_pointer.push_str("size ");
        lfs_pointer.push_str(&metadata.len().to_string());
        lfs_pointer.push_str("\n");

        // write the file to the .git/lfs/objects directory
        // create the directory recursively if it doesn't exist
        let lfs_objects_dir = repo.path().join("lfs/objects");
        std::fs::create_dir_all(lfs_objects_dir.clone())?;
        let lfs_path = lfs_objects_dir.join(sha);
        std::fs::copy(file_path, lfs_path)?;

        repo.blob(lfs_pointer.as_bytes()).unwrap()
    } else {
        // read the file into a blob, get the object id
        repo.blob_path(&file_path)?
    };

    // create a new IndexEntry from the file metadata
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(
                ctime.seconds().try_into()?,
                ctime.nanoseconds().try_into().unwrap(),
            ),
            mtime: git2::IndexTime::new(
                mtime.seconds().try_into()?,
                mtime.nanoseconds().try_into().unwrap(),
            ),
            dev: metadata.dev().try_into()?,
            ino: metadata.ino().try_into()?,
            mode: 33188,
            uid: metadata.uid().try_into().unwrap(),
            gid: metadata.gid().try_into().unwrap(),
            file_size: metadata.len().try_into().unwrap(),
            flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
            flags_extended: 0, // no extended flags
            path: rel_file_path.to_str().unwrap().to_string().into(),
            id: blob,
        })
        .with_context(|| format!("failed to add index entry for {}", file_path.display()))?;

    Ok(())
}

/// calculates sha256 digest of a large file as lowercase hex string via streaming buffer
/// used to calculate the hash of large files that are not supported by git
fn sha256_digest(path: &Path) -> Result<String> {
    let input = File::open(path)?;
    let mut reader = BufReader::new(input);

    let digest = {
        let mut hasher = Sha256::new();
        let mut buffer = [0; 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        hasher.finalize()
    };
    Ok(format!("{:X}", digest))
}

fn build_log_index(repo: &git2::Repository, index: &mut git2::Index) -> Result<()> {
    let logs_dir = repo.path().join("logs");
    if !logs_dir.exists() {
        return Ok(());
    }
    for log_file in fs::list_files(&logs_dir)? {
        let log_file = Path::new(&log_file);
        add_log_path(repo, index, &log_file)
            .with_context(|| format!("Failed to add log file to index: {}", log_file.display()))?;
    }
    Ok(())
}

fn add_log_path(
    repo: &git2::Repository,
    index: &mut git2::Index,
    rel_file_path: &Path,
) -> Result<()> {
    let file_path = repo.path().join("logs").join(rel_file_path);
    log::debug!("Adding log path: {}", file_path.display());

    let metadata = file_path.metadata()?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap_or(mtime);

    index.add(&git2::IndexEntry {
        ctime: git2::IndexTime::new(
            ctime.seconds().try_into()?,
            ctime.nanoseconds().try_into().unwrap(),
        ),
        mtime: git2::IndexTime::new(
            mtime.seconds().try_into()?,
            mtime.nanoseconds().try_into().unwrap(),
        ),
        dev: metadata.dev().try_into()?,
        ino: metadata.ino().try_into()?,
        mode: metadata.mode(),
        uid: metadata.uid().try_into().unwrap(),
        gid: metadata.gid().try_into().unwrap(),
        file_size: metadata.len().try_into()?,
        flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
        flags_extended: 0, // no extended flags
        path: rel_file_path.to_str().unwrap().to_string().into(),
        id: repo.blob_path(&file_path)?,
    })?;

    Ok(())
}

fn build_session_index(
    repo: &git2::Repository,
    project: &projects::Project,
    index: &mut git2::Index,
) -> Result<()> {
    // add all files in the working directory to the in-memory index, skipping for matching entries in the repo index
    let session_dir = project.session_path();
    for session_file in fs::list_files(&session_dir)? {
        let file_path = Path::new(&session_file);
        add_session_path(&repo, index, project, &file_path)
            .with_context(|| format!("failed to add session file: {}", file_path.display()))?;
    }

    Ok(())
}

// this is a helper function for build_gb_tree that takes paths under .git/gb/session and adds them to the in-memory index
fn add_session_path(
    repo: &git2::Repository,
    index: &mut git2::Index,
    project: &projects::Project,
    rel_file_path: &Path,
) -> Result<()> {
    let file_path = project.session_path().join(rel_file_path);

    log::debug!("adding session path: {}", file_path.display());

    let blob = repo.blob_path(&file_path)?;
    let metadata = file_path.metadata()?;
    let mtime = FileTime::from_last_modification_time(&metadata);
    let ctime = FileTime::from_creation_time(&metadata).unwrap_or(mtime);

    // create a new IndexEntry from the file metadata
    index
        .add(&git2::IndexEntry {
            ctime: git2::IndexTime::new(
                ctime.seconds().try_into()?,
                ctime.nanoseconds().try_into().unwrap(),
            ),
            mtime: git2::IndexTime::new(
                mtime.seconds().try_into()?,
                mtime.nanoseconds().try_into().unwrap(),
            ),
            dev: metadata.dev().try_into()?,
            ino: metadata.ino().try_into()?,
            mode: metadata.mode(),
            uid: metadata.uid().try_into().unwrap(),
            gid: metadata.gid().try_into().unwrap(),
            file_size: metadata.len().try_into()?,
            flags: 10, // normal flags for normal file (for the curious: https://git-scm.com/docs/index-format)
            flags_extended: 0, // no extended flags
            path: rel_file_path.to_str().unwrap().into(),
            id: blob,
        })
        .with_context(|| {
            format!(
                "Failed to add session file to index: {}",
                file_path.display()
            )
        })?;

    Ok(())
}

// write a new commit object to the repo
// this is called once we have a tree of deltas, metadata and current wd snapshot
// and either creates or updates the refs/gitbutler/current ref
fn write_gb_commit(
    gb_tree: git2::Oid,
    repo: &git2::Repository,
    user: &Option<users::User>,
    project: &projects::Project,
) -> Result<git2::Oid> {
    // find the Oid of the commit that refs/.../current points to, none if it doesn't exist
    let refname = project.refname();

    let comitter = git2::Signature::now("gitbutler", "gitbutler@localhost")?;

    let author = match user {
        None => comitter.clone(),
        Some(user) => git2::Signature::now(user.name.as_str(), user.email.as_str())?,
    };

    match repo.revparse_single(refname.as_str()) {
        Ok(obj) => {
            let last_commit = repo.find_commit(obj.id()).unwrap();
            let new_commit = repo.commit(
                Some(refname.as_str()),
                &author,                           // author
                &comitter,                         // committer
                "gitbutler check",                 // commit message
                &repo.find_tree(gb_tree).unwrap(), // tree
                &[&last_commit],                   // parents
            )?;
            Ok(new_commit)
        }
        Err(e) => {
            if e.code() == git2::ErrorCode::NotFound {
                let new_commit = repo.commit(
                    Some(refname.as_str()),
                    &author,                           // author
                    &comitter,                         // committer
                    "gitbutler check",                 // commit message
                    &repo.find_tree(gb_tree).unwrap(), // tree
                    &[],                               // parents
                )?;
                Ok(new_commit)
            } else {
                return Err(e.into());
            }
        }
    }
}
