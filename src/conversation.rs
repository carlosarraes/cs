use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, IsTerminal, Write},
    path::Path,
};

type Conversation = (String, Option<String>, std::time::SystemTime);

fn catalog(source: &Path, cwd: &Path) -> Result<Vec<Conversation>> {
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

fn log_paths(root: &str, source: &Path) -> Result<Vec<std::path::PathBuf>> {
    if Path::new(root).file_name().and_then(|s| s.to_str()) != Some(root) {
        bail!("invalid conversation id");
    }
    let entries = match fs::read_dir(source.join("projects")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path().join(format!("{root}.jsonl"));
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => paths.push(path),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(paths)
}

pub fn has_log(root: &str, source: &Path) -> Result<bool> {
    Ok(!log_paths(root, source)?.is_empty())
}

pub fn has_usage(root: &str, source: &Path) -> Result<bool> {
    for path in log_paths(root, source)? {
        let file = fs::File::open(path)?;
        for line in BufReader::new(file).lines() {
            let line = line?;
            if line.contains("\"usage\"") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn resolve(args: &mut Vec<OsString>, source: &Path, cwd: &Path) -> Result<String> {
    let stdin = std::io::stdin();
    resolve_with_io(
        args,
        source,
        cwd,
        stdin.is_terminal(),
        &mut stdin.lock(),
        &mut std::io::stderr().lock(),
    )
}

fn conversation_label((id, title, modified): &Conversation) -> String {
    let modified: chrono::DateTime<chrono::Local> = (*modified).into();
    format!(
        "{id} ({}, {})",
        title.as_deref().unwrap_or("Untitled"),
        modified.format("%Y-%m-%d %H:%M")
    )
}

fn pick_conversation(
    entries: &[Conversation],
    terminal: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<String> {
    if !terminal {
        bail!("headless resume requires a conversation name or UUID");
    }
    for (i, entry) in entries.iter().enumerate() {
        writeln!(output, "{}: {}", i + 1, conversation_label(entry))?;
    }
    write!(output, "Resume number: ")?;
    output.flush()?;
    let mut choice = String::new();
    input.read_line(&mut choice)?;
    let index = choice
        .trim()
        .parse::<usize>()
        .context("enter a conversation number")?
        .checked_sub(1)
        .context("invalid conversation number")?;
    Ok(entries
        .get(index)
        .context("invalid conversation number")?
        .0
        .clone())
}

fn resolve_with_io(
    args: &mut Vec<OsString>,
    source: &Path,
    cwd: &Path,
    terminal: bool,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> Result<String> {
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
                let entries = catalog(source, cwd)?;
                let mut matches: Vec<_> = entries
                    .iter()
                    .filter(|(_, title, _)| title.as_deref() == Some(&name))
                    .cloned()
                    .collect();
                let exact_count = matches.len();
                if exact_count == 1 {
                    matches[0].0.clone()
                } else {
                    if matches.is_empty() {
                        let query = name.to_lowercase();
                        matches = entries
                            .iter()
                            .filter(|(_, title, _)| {
                                title
                                    .as_ref()
                                    .is_some_and(|title| title.to_lowercase().contains(&query))
                            })
                            .cloned()
                            .collect();
                        if matches.is_empty() {
                            matches = entries;
                        }
                    }
                    if matches.is_empty() {
                        bail!("no conversations to resume in this project");
                    }
                    if !terminal {
                        let candidates = matches
                            .iter()
                            .map(conversation_label)
                            .collect::<Vec<_>>()
                            .join(", ");
                        bail!("resume name {name:?} matched {exact_count} conversations in this project; pass one of: {candidates}");
                    }
                    pick_conversation(&matches, terminal, input, output)?
                }
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
                pick_conversation(&entries, terminal, input, output)?
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

    const NEWER: &str = "11111111-1111-4111-8111-111111111111";
    const OLDER: &str = "22222222-2222-4222-8222-222222222222";

    fn write_conversation(source: &Path, id: &str, title: &str, modified: u64) {
        let project = source.join("projects/-work-repo");
        fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{id}.jsonl"));
        fs::write(
            &path,
            serde_json::json!({"type": "custom-title", "customTitle": title, "sessionId": id})
                .to_string(),
        )
        .unwrap();
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified)),
            )
            .unwrap();
    }

    #[test]
    fn ambiguous_named_resume_lists_candidate_uuids_newest_first_without_a_terminal() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "cos", 1_000_000);
        write_conversation(d.path(), NEWER, "cos", 2_000_000);
        for selector in [
            vec!["--resume", "cos"],
            vec!["-r", "cos"],
            vec!["--resume=cos"],
        ] {
            let mut args: Vec<OsString> = selector.iter().map(OsString::from).collect();
            let original = args.clone();
            let mut output = Vec::new();
            let error = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                false,
                &mut &b""[..],
                &mut output,
            )
            .unwrap_err()
            .to_string();

            assert!(
                error.contains(
                    "resume name \"cos\" matched 2 conversations in this project; pass one of:"
                ),
                "{error}"
            );
            let newer = error.find(&format!("{NEWER} (cos, ")).unwrap();
            let older = error.find(&format!("{OLDER} (cos, ")).unwrap();
            assert!(newer < older, "{error}");
            assert!(output.is_empty());
            assert_eq!(args, original);
        }
    }

    #[test]
    fn ambiguous_named_resume_picker_can_select_the_older_exact_match() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "cos", 1_000_000);
        write_conversation(d.path(), NEWER, "cos", 2_000_000);
        let unrelated = "33333333-3333-4333-8333-333333333333";
        write_conversation(d.path(), unrelated, "cos notes", 3_000_000);

        for selector in [
            vec!["--resume", "cos"],
            vec!["-r", "cos"],
            vec!["--resume=cos"],
        ] {
            let mut args: Vec<OsString> = selector.into_iter().map(OsString::from).collect();
            let mut output = Vec::new();
            let id = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                true,
                &mut &b"2\n"[..],
                &mut output,
            )
            .unwrap();

            assert_eq!(id, OLDER);
            assert_eq!(
                args,
                vec![OsString::from("--resume"), OsString::from(OLDER)]
            );
            let output = String::from_utf8(output).unwrap();
            assert!(output.find(NEWER).unwrap() < output.find(OLDER).unwrap());
            assert!(!output.contains(unrelated), "{output}");
            let local_time = chrono::DateTime::parse_from_rfc3339("1970-01-24T03:33:20Z")
                .unwrap()
                .with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string();
            assert!(output.contains(&local_time), "{output}");
            assert!(output.ends_with("Resume number: "), "{output}");
        }
    }

    #[test]
    fn unique_named_resume_does_not_prompt_even_with_similar_titles() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "cos", 1_000_000);
        write_conversation(d.path(), NEWER, "cos notes", 2_000_000);
        for terminal in [false, true] {
            let mut args = vec!["--resume".into(), "cos".into()];
            let mut output = Vec::new();
            let id = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                terminal,
                &mut &b""[..],
                &mut output,
            )
            .unwrap();
            assert_eq!(id, OLDER);
            assert_eq!(args[1], OLDER);
            assert!(output.is_empty());
        }
    }

    #[test]
    fn missing_exact_title_picker_only_shows_case_insensitive_substring_matches() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "COS work", 1_000_000);
        write_conversation(d.path(), NEWER, "other", 2_000_000);
        let mut args = vec!["--resume".into(), "cos".into()];
        let mut output = Vec::new();
        let id = resolve_with_io(
            &mut args,
            d.path(),
            Path::new("/work/repo"),
            true,
            &mut &b"1\n"[..],
            &mut output,
        )
        .unwrap();

        assert_eq!(id, OLDER);
        assert_eq!(args[1], OLDER);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("COS work"), "{output}");
        assert!(output.contains(OLDER), "{output}");
        assert!(!output.contains(NEWER), "{output}");
    }

    #[test]
    fn missing_title_picker_falls_back_to_the_full_project_list() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "old", 1_000_000);
        write_conversation(d.path(), NEWER, "new", 2_000_000);
        for selector in [vec!["--resume", "missing"], vec!["--resume"]] {
            let mut args: Vec<OsString> = selector.into_iter().map(OsString::from).collect();
            let mut output = Vec::new();
            let id = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                true,
                &mut &b"2\n"[..],
                &mut output,
            )
            .unwrap();
            assert_eq!(id, OLDER);
            assert_eq!(args[1], OLDER);
            let output = String::from_utf8(output).unwrap();
            assert!(output.find(NEWER).unwrap() < output.find(OLDER).unwrap());
        }
    }

    #[test]
    fn missing_exact_title_without_a_terminal_lists_search_candidates() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "COS work", 1_000_000);
        write_conversation(d.path(), NEWER, "other", 2_000_000);
        for (name, includes_unrelated) in [("cos", false), ("missing", true)] {
            let mut args = vec!["--resume".into(), name.into()];
            let mut output = Vec::new();
            let error = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                false,
                &mut &b""[..],
                &mut output,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("matched 0 conversations"), "{error}");
            assert!(error.contains(&format!("{OLDER} (COS work, ")), "{error}");
            assert_eq!(error.contains(NEWER), includes_unrelated, "{error}");
            if includes_unrelated {
                assert!(error.find(NEWER).unwrap() < error.find(OLDER).unwrap());
            }
            assert!(output.is_empty());
        }
    }

    #[test]
    fn resume_picker_rejects_invalid_or_missing_choices_without_selecting_a_session() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "cos", 1_000_000);
        write_conversation(d.path(), NEWER, "cos", 2_000_000);
        for choice in ["", "\n", "0\n", "3\n", "invalid\n"] {
            let mut args = vec!["--resume".into(), "cos".into()];
            let original = args.clone();
            let error = resolve_with_io(
                &mut args,
                d.path(),
                Path::new("/work/repo"),
                true,
                &mut choice.as_bytes(),
                &mut Vec::new(),
            )
            .unwrap_err();
            assert!(error.to_string().contains("conversation number"), "{error}");
            assert_eq!(args, original);
        }
    }

    #[test]
    fn continue_flags_select_the_newest_session_without_prompting() {
        let d = tempfile::tempdir().unwrap();
        write_conversation(d.path(), OLDER, "cos", 1_000_000);
        write_conversation(d.path(), NEWER, "cos", 2_000_000);
        for flag in ["--continue", "-c"] {
            for terminal in [false, true] {
                let mut args = vec![flag.into()];
                let mut output = Vec::new();
                let id = resolve_with_io(
                    &mut args,
                    d.path(),
                    Path::new("/work/repo"),
                    terminal,
                    &mut &b""[..],
                    &mut output,
                )
                .unwrap();
                assert_eq!(id, NEWER);
                assert_eq!(
                    args,
                    vec![OsString::from("--resume"), OsString::from(NEWER)]
                );
                assert!(output.is_empty());
            }
        }
    }

    #[test]
    fn attribution_guard_ignores_metadata_files_and_fresh_session_headers() {
        let d = tempfile::tempdir().unwrap();
        let project = d.path().join("projects/project");
        fs::create_dir_all(&project).unwrap();
        fs::write(d.path().join("projects/.DS_Store"), "metadata").unwrap();
        fs::write(
            project.join("fresh.jsonl"),
            r#"{"type":"system","subtype":"session_start"}"#,
        )
        .unwrap();
        assert!(!has_usage("fresh", d.path()).unwrap());
        fs::write(
            project.join("old.jsonl"),
            r#"{"message":{"usage":{"input_tokens":1}}}"#,
        )
        .unwrap();
        assert!(has_usage("old", d.path()).unwrap());
    }
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
        assert!(resolve_with_io(
            &mut missing,
            d.path(),
            Path::new("/work/repo"),
            false,
            &mut &b""[..],
            &mut Vec::new(),
        )
        .is_err());
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
