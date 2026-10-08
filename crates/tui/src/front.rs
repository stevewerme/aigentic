//! The front thread (issue #89): the pick a plain `aigentic` makes, the
//! project and role a picked thread carries, and the banner's thread
//! line. `main.rs` wires these; the daemon side is #84's.

use aigentic_api::client::Client;
use aigentic_api::{FrontOutcome, ProjectInfo, Request, Response, StartAsk, ThreadInfo};
use aigentic_runtime::Project;
use aigentic_runtime::aigentic_core::Event;
use ulid::Ulid;

use crate::threads_index;

/// The thread a launch is about, and what the daemon said about it.
#[derive(Debug)]
pub struct Picked {
    pub id: Ulid,
    /// The listing row `CreateThread` and `Front` answer with; `None`
    /// for an explicit `--thread`, whose own events say what it is.
    pub info: Option<ThreadInfo>,
    /// How `Front` reached the thread; `None` for `--thread` and `exec`.
    pub outcome: Option<FrontOutcome>,
}

/// The folder's project for the start-up switch proposal (issue #92):
/// `--project`, else the folder's `aigentic.toml` name. Never a bare
/// folder's basename, and never `Welcome`'s first project: a bare folder
/// resolves to `None` and proposes nothing.
pub fn here_project(project: Option<&str>, opened: Option<&Project>) -> Option<String> {
    project
        .map(str::to_owned)
        .or_else(|| opened.map(|p| p.name.clone()))
}

/// Pick the thread to open (issue #89): the one named, `exec`'s own new
/// thread, or the user's front thread, which a plain launch resumes.
/// `here` is the folder's project (issue #92), sent on `Front` only so
/// the daemon can offer a switch; `exec` and `--thread` never send it.
/// `asked` is the start-up ask's answer (issue #121), sent with `Front`
/// so the daemon honours it and logs it as a decision.
pub async fn pick_thread(
    client: &Client,
    thread: Option<Ulid>,
    exec: bool,
    project: &str,
    here: Option<&str>,
    asked: Option<StartAsk>,
) -> anyhow::Result<Picked> {
    if let Some(id) = thread {
        return Ok(Picked {
            id,
            info: None,
            outcome: None,
        });
    }
    let request = if exec {
        Request::CreateThread {
            project: project.to_owned(),
        }
    } else {
        Request::Front {
            project: project.to_owned(),
            here: here.map(str::to_owned),
            asked,
        }
    };
    match client.request(request).await? {
        Response::Thread { thread } => Ok(Picked {
            id: thread.id,
            info: Some(thread),
            outcome: None,
        }),
        Response::Front { thread, outcome } => Ok(Picked {
            id: thread.id,
            info: Some(thread),
            outcome: Some(outcome),
        }),
        Response::Refused { reason } => {
            let reason = without_project(&reason, project);
            anyhow::bail!("cannot start a thread in {project}: {reason}")
        }
        other => anyhow::bail!("unexpected reply starting a thread: {other:?}"),
    }
}

/// The project a picked thread is in (issue #89, design 2): its listing
/// row, else its own events (a `--thread` pick, or a switch), else the
/// project the client would have created in.
pub fn thread_project(picked: &Picked, events: &[Event], project_name: &str) -> String {
    if let Some(project) = picked.info.as_ref().and_then(|info| info.project.clone()) {
        return project;
    }
    threads_index::project_of(events, None, &[]).unwrap_or_else(|| project_name.to_owned())
}

/// The title to show for a thread (issue #89): the recorded one, else
/// its first line when it has one.
pub fn title_of(info: &ThreadInfo) -> Option<String> {
    if let Some(title) = &info.title {
        return Some(title.clone());
    }
    (!info.first_line.is_empty()).then(|| info.first_line.clone())
}

/// The user's role in `project`, from `Welcome`'s or `ListProjects`'
/// rows: the role a request in that project needs is a daemon question,
/// so the client only reads what the daemon said.
pub fn role_for(projects: &[ProjectInfo], project: &str) -> Option<String> {
    projects
        .iter()
        .find(|p| p.name == project)
        .and_then(|p| p.role.clone())
}

/// The banner's thread line (issue #89, design 3): what the pick came
/// to, in the terminal and in plain mode. `outcome` is `None` for a
/// `--thread` pick, which keeps today's line.
pub fn front_line(
    outcome: Option<&FrontOutcome>,
    info: Option<&ThreadInfo>,
    id: Ulid,
    events: usize,
    plain: bool,
) -> String {
    let title = info.and_then(title_of);
    match outcome {
        None => {
            if plain {
                format!("thread {id} resumed with {events} events")
            } else {
                format!("resumed {id} · {events} events")
            }
        }
        Some(FrontOutcome::Resumed) => {
            let name = match title {
                Some(title) => format!("\"{title}\""),
                None => id.to_string(),
            };
            if plain {
                format!("front thread {name} resumed with {events} events")
            } else {
                format!("front thread {name} · {events} events")
            }
        }
        Some(FrontOutcome::First) => {
            if plain {
                format!("new front thread {id} (aigentic resumes it next time)")
            } else {
                "new front thread · aigentic resumes it next time".to_owned()
            }
        }
        Some(FrontOutcome::Replaced { reason }) => {
            if plain {
                format!("new front thread {id}: {reason}")
            } else {
                format!("new front thread: {reason}")
            }
        }
    }
}
/// A refusal's reason without the `in {project}: ` the daemon may have
/// put in front of it already (`Front`'s create check does; the generic
/// pre-check does not), so the client's own prefix names the project
/// once.
pub fn without_project<'a>(reason: &'a str, project: &str) -> &'a str {
    reason
        .strip_prefix("in ")
        .and_then(|rest| rest.strip_prefix(project))
        .and_then(|rest| rest.strip_prefix(": "))
        .unwrap_or(reason)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::rig::{daemon, open, project};
    use aigentic_api::client::Client;
    use aigentic_api::{Request, Response};
    use aigentic_runtime::aigentic_core::Event;
    use ulid::Ulid;

    /// T2: the pick's four shapes.
    #[tokio::test]
    async fn a_first_pick_makes_the_front_thread_and_the_next_resumes_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let addr = daemon(dir.path(), &[("steve", "t")], &[("p", root)]).await;
        let (client, _) = Client::connect(&addr, "t").await.unwrap();
        let first = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        assert_eq!(first.outcome, Some(FrontOutcome::First));
        assert_eq!(
            first.info.as_ref().unwrap().project.as_deref(),
            Some("p"),
            "a new front thread is in the project asked for"
        );
        let again = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        assert_eq!(again.outcome, Some(FrontOutcome::Resumed));
        assert_eq!(again.id, first.id);
    }

    /// T2: `exec` never touches the front thread.
    #[tokio::test]
    async fn exec_gets_a_new_thread_and_the_front_thread_still_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let addr = daemon(dir.path(), &[("steve", "t")], &[("p", root)]).await;
        let (client, _) = Client::connect(&addr, "t").await.unwrap();
        let front = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        let exec = pick_thread(&client, None, true, "p", None, None)
            .await
            .unwrap();
        assert!(exec.outcome.is_none());
        assert!(exec.info.is_some());
        assert_ne!(exec.id, front.id);
        let again = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        assert_eq!(again.outcome, Some(FrontOutcome::Resumed));
        assert_eq!(again.id, front.id);
    }

    /// T2: `--thread X` opens X and leaves the front thread alone.
    #[tokio::test]
    async fn an_explicit_thread_is_opened_and_the_front_thread_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "p", "");
        let addr = daemon(dir.path(), &[("steve", "t")], &[("p", root)]).await;
        let (client, _) = Client::connect(&addr, "t").await.unwrap();
        let front = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        let (other, _, _) = open(&client, "p", None).await;
        let picked = pick_thread(&client, Some(other), false, "p", None, None)
            .await
            .unwrap();
        assert_eq!(picked.id, other);
        assert!(picked.info.is_none());
        assert!(picked.outcome.is_none());
        let again = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap();
        assert_eq!(again.id, front.id);
    }

    /// T2: a read-only user cannot start a front thread.
    #[tokio::test]
    async fn a_read_only_user_cannot_start_a_thread() {
        let dir = tempfile::tempdir().unwrap();
        let root = project(
            dir.path(),
            "p",
            "[participants]\nsteve = \"admin\"\nmagnus = \"read\"\n",
        );
        let addr = daemon(
            dir.path(),
            &[("steve", "s"), ("magnus", "m")],
            &[("p", root)],
        )
        .await;
        let (client, _) = Client::connect(&addr, "m").await.unwrap();
        let err = pick_thread(&client, None, false, "p", None, None)
            .await
            .unwrap_err()
            .to_string();
        let reason = err
            .strip_prefix("cannot start a thread in p: ")
            .unwrap_or_else(|| panic!("the reason is named separately: {err}"));
        assert!(!reason.is_empty(), "the daemon's reason is there: {err}");
    }

    /// T3: the REPL is in the thread's project, not the folder's, and the
    /// role is that project's row.
    #[tokio::test]
    async fn a_front_threads_project_and_role_win_over_the_folders() {
        let dir = tempfile::tempdir().unwrap();
        let a = project(
            dir.path(),
            "a",
            "[participants]\nsteve = \"admin\"\nmagnus = \"admin\"\n",
        );
        let b = project(
            dir.path(),
            "b",
            "[participants]\nsteve = \"admin\"\nmagnus = \"read\"\n",
        );
        let addr = daemon(
            dir.path(),
            &[("steve", "s"), ("magnus", "m")],
            &[("a", a), ("b", b)],
        )
        .await;
        // First pick, standing in `a`: the front thread is created there.
        let (first_client, _) = Client::connect(&addr, "m").await.unwrap();
        let first = pick_thread(&first_client, None, false, "a", None, None)
            .await
            .unwrap();
        assert_eq!(first.outcome, Some(FrontOutcome::First));

        // Now standing in `b`: the front thread is still the one in `a`.
        let (client, welcome) = Client::connect(&addr, "m").await.unwrap();
        let picked = pick_thread(&client, None, false, "b", None, None)
            .await
            .unwrap();
        assert_eq!(picked.outcome, Some(FrontOutcome::Resumed));
        assert_eq!(picked.id, first.id);
        let events = events_of(&client, picked.id).await;
        assert_eq!(thread_project(&picked, &events, "b"), "a");
        assert_eq!(
            role_for(&welcome.projects, "a").as_deref(),
            Some("admin"),
            "the thread's project decides the role"
        );
        assert_eq!(
            role_for(&welcome.projects, "b").as_deref(),
            Some("read"),
            "not the folder's project's row"
        );
    }

    /// T3: a `--thread` pick has no listing row, so its own events say
    /// which project it is in after a switch.
    #[tokio::test]
    async fn a_switched_thread_resolves_from_its_events() {
        let dir = tempfile::tempdir().unwrap();
        let a = project(dir.path(), "a", "[participants]\nsteve = \"admin\"\n");
        let b = project(dir.path(), "b", "[participants]\nsteve = \"admin\"\n");
        let addr = daemon(dir.path(), &[("steve", "s")], &[("a", a), ("b", b)]).await;
        let (client, _) = Client::connect(&addr, "s").await.unwrap();
        let (id, _, _) = open(&client, "b", None).await;
        let moved = client
            .request(Request::SwitchProject {
                thread: id,
                project: "a".into(),
            })
            .await
            .unwrap();
        assert!(
            matches!(moved, Response::Ok),
            "the switch landed: {moved:?}"
        );
        let picked = pick_thread(&client, Some(id), false, "b", None, None)
            .await
            .unwrap();
        assert!(picked.info.is_none(), "a --thread pick has no listing");
        let events = events_of(&client, id).await;
        assert_eq!(thread_project(&picked, &events, "b"), "a");
    }

    /// T3: with no row and no switch, the folder's project is the
    /// fallback.
    #[test]
    fn thread_project_falls_back_to_the_folders_project() {
        let picked = Picked {
            id: Ulid::from_parts(1, 1),
            info: None,
            outcome: None,
        };
        assert_eq!(thread_project(&picked, &[], "b"), "b");
        assert_eq!(thread_project(&picked, &[], "p"), "p");
    }

    /// T6 (issue #92): `here` is `--project`, else the folder's
    /// `aigentic.toml` name, and `None` for a bare folder — never a
    /// basename or `Welcome`'s first project. Run outside any project:
    /// `Project::open` walks up, so a tempdir under one is not bare.
    #[test]
    fn t6_here_is_the_folders_project_and_a_bare_folder_sends_none() {
        let bare = tempfile::tempdir().unwrap();
        assert!(
            Project::open(bare.path()).unwrap().is_none(),
            "a bare folder opens no project"
        );
        assert_eq!(
            here_project(None, None),
            None,
            "a bare folder proposes nothing"
        );
        assert_eq!(
            here_project(Some("p"), None).as_deref(),
            Some("p"),
            "`--project` wins even in a bare folder"
        );

        let dir = tempfile::tempdir().unwrap();
        let root = project(dir.path(), "b", "");
        let opened = Project::open(&root).unwrap().unwrap();
        assert_eq!(
            here_project(None, Some(&opened)).as_deref(),
            Some("b"),
            "else the folder's own aigentic.toml name"
        );
        assert_eq!(
            here_project(Some("p"), Some(&opened)).as_deref(),
            Some("p"),
            "`--project` still wins over the folder"
        );
    }

    /// T4: every row of the banner table, in both modes.
    #[test]
    fn the_banner_line_says_which_thread_you_are_in() {
        let id = Ulid::from_parts(7, 8);
        let n = 3usize;
        let titled = info(id, Some("T"), "first line");
        let untitled = info(id, None, "");
        let first_line_only = info(id, None, "first line");
        let reason = "the old front thread has no project this daemon can open";
        let replaced = FrontOutcome::Replaced {
            reason: reason.to_owned(),
        };

        for (outcome, row, terminal, plain) in [
            (
                Some(FrontOutcome::Resumed),
                titled.clone(),
                format!("front thread \"T\" · {n} events"),
                format!("front thread \"T\" resumed with {n} events"),
            ),
            (
                Some(FrontOutcome::Resumed),
                untitled.clone(),
                format!("front thread {id} · {n} events"),
                format!("front thread {id} resumed with {n} events"),
            ),
            (
                Some(FrontOutcome::First),
                untitled.clone(),
                "new front thread · aigentic resumes it next time".to_owned(),
                format!("new front thread {id} (aigentic resumes it next time)"),
            ),
            (
                Some(replaced.clone()),
                untitled.clone(),
                format!("new front thread: {reason}"),
                format!("new front thread {id}: {reason}"),
            ),
            (
                None,
                untitled.clone(),
                format!("resumed {id} · {n} events"),
                format!("thread {id} resumed with {n} events"),
            ),
        ] {
            assert_eq!(
                front_line(outcome.as_ref(), Some(&row), id, n, false),
                terminal,
                "terminal, {outcome:?}"
            );
            assert_eq!(
                front_line(outcome.as_ref(), Some(&row), id, n, true),
                plain,
                "plain, {outcome:?}"
            );
        }

        // A title that is only a first line reads the same way.
        assert_eq!(
            front_line(
                Some(&FrontOutcome::Resumed),
                Some(&first_line_only),
                id,
                n,
                false
            ),
            format!("front thread \"first line\" · {n} events")
        );
        // A `Front` reply is always there for `Resumed` and `First`; a
        // missing one still draws a line rather than nothing.
        assert_eq!(
            front_line(Some(&FrontOutcome::Resumed), None, id, n, false),
            format!("front thread {id} · {n} events")
        );
    }

    fn info(id: Ulid, title: Option<&str>, first_line: &str) -> ThreadInfo {
        ThreadInfo {
            id,
            project: Some("p".into()),
            date: "2026-10-05".into(),
            events: 0,
            first_line: first_line.into(),
            state: aigentic_api::ThreadState::Idle,
            title: title.map(str::to_owned),
            workspace: None,
            kind: aigentic_api::ThreadKind::Thread,
        }
    }

    async fn events_of(client: &Client, thread: Ulid) -> Vec<Event> {
        match client
            .request(Request::Open {
                thread,
                from_seq: 0,
            })
            .await
            .unwrap()
        {
            Response::Opened { events, .. } => events,
            other => panic!("{other:?}"),
        }
    }

    /// The project is named once, whether or not the daemon's reason
    /// already carried it.
    #[test]
    fn a_refusal_names_the_project_once() {
        let denied = "cara is read in this project; this needs write";
        assert_eq!(without_project(&format!("in ro: {denied}"), "ro"), denied);
        assert_eq!(without_project(denied, "ro"), denied);
        // Another project's prefix is part of the reason, not ours.
        let other = format!("in rw: {denied}");
        assert_eq!(without_project(&other, "ro"), other);
    }
}
