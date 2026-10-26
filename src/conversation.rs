use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader},
    path::Path,
};

fn catalog(
    source: &Path,
    cwd: &Path,
) -> Result<Vec<(String, Option<String>, std::time::SystemTime)>> {
    let project: String = cwd
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let dir = source.join("projects").join(project);
    let mut sessions = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(sessions),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let id = path.file_stem().and_then(|x| x.to_str()).unwrap_or("");
        if uuid::Uuid::parse_str(id).is_err() {
            continue;
        }
        let mut title = None;
        for line in BufReader::new(fs::File::open(&path)?).lines() {
            let line = line?;
            if !line.contains("customTitle") {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if let Some(name) = value.get("customTitle").and_then(Value::as_str) {
                    title = Some(name.to_string());
                }
            }
        }
        sessions.push((id.to_string(), title, fs::metadata(&path)?.modified()?));
    }
    sessions.sort_by_key(|a| std::cmp::Reverse(a.2));
    Ok(sessions)
}

pub fn ensure_not_running(root: &str, source: &Path) -> Result<()> {
    let entries = match fs::read_dir(source.join("sessions")) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    for entry in entries.flatten() {
        let Ok(bytes) = fs::read(entry.path()) else {
            continue;
        };
        let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        if value["sessionId"].as_str() != Some(root) {
            continue;
        }
        if let Some(pid) = value["pid"].as_u64().and_then(|p| u32::try_from(p).ok()) {
            if crate::routing::sessions::Process::read(pid).is_some() {
                bail!("conversation {root} is already running in PID {pid}; close that conversation before resuming it");
            }
        }
    }
    Ok(())
}

pub fn resolve(args: &mut Vec<OsString>, source: &Path, cwd: &Path) -> Result<String> {
    let mut selector = None;
    let mut explicit = None;
    let mut fork = false;
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].to_string_lossy();
        if arg == "--" {
            break;
        }
        if arg == "--fork-session" {
            fork = true;
        }
        if matches!(arg.as_ref(), "--session-id" | "--resume" | "-r") {
            let key = arg.into_owned();
            let next = args
                .get(i + 1)
                .filter(|v| !v.to_string_lossy().starts_with('-'))
                .map(|v| v.to_string_lossy().into_owned());
            if key == "--session-id" {
                explicit = next.clone();
            } else {
                selector = Some((i, next.clone(), if next.is_some() { 2 } else { 1 }));
            }
            if next.is_some() {
                i += 1;
            }
        } else if let Some(value) = arg.strip_prefix("--resume=") {
            selector = Some((i, Some(value.to_string()), 1));
        } else if let Some(value) = arg.strip_prefix("--session-id=") {
            explicit = Some(value.to_string());
        } else if matches!(arg.as_ref(), "--continue" | "-c") {
            selector = Some((i, Some(String::new()), 1));
        }
        i += 1;
    }
    let resumed = if let Some((index, name, remove)) = selector {
        let id = if let Some(name) = name.filter(|n| !n.is_empty()) {
            if uuid::Uuid::parse_str(&name).is_ok() {
                name
            } else {
                let matches: Vec<_> = catalog(source, cwd)?
                    .into_iter()
                    .filter(|(_, title, _)| title.as_deref() == Some(&name))
                    .collect();
                if matches.len() != 1 {
                    bail!("resume name {name:?} matched {} conversations in this project; pass a session UUID",matches.len());
                }
                matches[0].0.clone()
            }
        } else {
            let entries = catalog(source, cwd)?;
            if entries.is_empty() {
                bail!("no conversations to resume in this project");
            }
            let continuing = matches!(args[index].to_str(), Some("--continue" | "-c"));
            if continuing {
                entries[0].0.clone()
            } else {
                use std::io::{IsTerminal, Write};
                if !std::io::stdin().is_terminal() {
                    bail!("headless resume requires a conversation name or UUID");
                }
                for (i, (id, title, _)) in entries.iter().enumerate() {
                    eprintln!(
                        "{}: {} ({id})",
                        i + 1,
                        title.as_deref().unwrap_or("Untitled")
                    );
                }
                eprint!("Resume number: ");
                std::io::stderr().flush()?;
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                let index = input
                    .trim()
                    .parse::<usize>()
                    .context("enter a conversation number")?
                    .checked_sub(1)
                    .context("invalid conversation number")?;
                entries
                    .get(index)
                    .context("invalid conversation number")?
                    .0
                    .clone()
            }
        };
        args.splice(
            index..index + remove,
            [OsString::from("--resume"), OsString::from(&id)],
        );
        Some(id)
    } else {
        None
    };
    if let Some(id) = explicit {
        uuid::Uuid::parse_str(&id).context("invalid --session-id")?;
        return Ok(id);
    }
    if !fork {
        if let Some(id) = resumed {
            return Ok(id);
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    let index = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    args.splice(
        index..index,
        [OsString::from("--session-id"), OsString::from(&id)],
    );
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn continuing_and_forking_preserve_history_but_allocate_the_right_root() {
        let d = tempfile::tempdir().unwrap();
        let project = d.path().join("projects/-work");
        fs::create_dir_all(&project).unwrap();
        let original = uuid::Uuid::new_v4().to_string();
        fs::write(project.join(format!("{original}.jsonl")), "{}\n").unwrap();
        let mut continuing = vec!["--continue".into()];
        assert_eq!(
            resolve(&mut continuing, d.path(), Path::new("/work")).unwrap(),
            original
        );
        assert_eq!(
            continuing,
            vec![OsString::from("--resume"), OsString::from(&original)]
        );
        let mut fork = vec![
            "--resume".into(),
            original.clone().into(),
            "--fork-session".into(),
        ];
        let root = resolve(&mut fork, d.path(), Path::new("/work")).unwrap();
        assert_ne!(root, original);
        assert_eq!(fork[1], original.as_str());
        assert_eq!(fork.last().unwrap(), &OsString::from(root));
    }

    #[test]
    fn running_conversation_is_rejected_without_affecting_its_process() {
        let d = tempfile::tempdir().unwrap();
        fs::create_dir(d.path().join("sessions")).unwrap();
        fs::write(
            d.path().join("sessions/live.json"),
            serde_json::json!({"sessionId":"pm-root", "pid":std::process::id()}).to_string(),
        )
        .unwrap();
        assert!(ensure_not_running("pm-root", d.path()).is_err());
        assert!(ensure_not_running("another-root", d.path()).is_ok());
        assert!(crate::routing::sessions::Process::read(std::process::id()).is_some());
    }
    #[test]
    fn named_resume_uses_exact_title_in_current_project_and_rewrites_to_uuid() {
        let d = tempfile::tempdir().unwrap();
        let project = d.path().join("projects").join("-work-repo");
        fs::create_dir_all(&project).unwrap();
        let id = "63bb36aa-75b6-40bc-86aa-15287914e6f9";
        fs::write(
            project.join(format!("{id}.jsonl")),
            format!(
                "{{\"type\":\"custom-title\",\"customTitle\":\"pm\",\"sessionId\":\"{id}\"}}\n"
            ),
        )
        .unwrap();
        let mut args = vec!["--chrome".into(), "--resume".into(), "pm".into()];
        assert_eq!(
            resolve(&mut args, d.path(), Path::new("/work/repo")).unwrap(),
            id
        );
        assert_eq!(args[2], id);
        let mut missing = vec!["--resume".into(), "missing".into()];
        assert!(resolve(&mut missing, d.path(), Path::new("/work/repo")).is_err());
    }
    #[test]
    fn new_sessions_get_an_explicit_uuid_and_explicit_resumes_are_preserved() {
        let d = tempfile::tempdir().unwrap();
        let mut args = vec!["-p".into(), "hi".into()];
        let id = resolve(&mut args, d.path(), Path::new("/work")).unwrap();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert_eq!(args[args.len() - 1], id.as_str());
        let mut args = vec!["--resume".into(), id.clone().into()];
        assert_eq!(
            resolve(&mut args, d.path(), Path::new("/work")).unwrap(),
            id
        );
    }
}
