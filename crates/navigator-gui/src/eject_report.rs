//! The words for a failed eject: what refused, and who is holding the
//! drive. Pure, so the phrasing is unit-tested without a device.

use navigator_fs::{DriveUser, EjectFailure};

pub struct Report {
    /// One or two sentences for the screen reader.
    pub spoken: String,
    /// The dialog body: the reason, then one block per process.
    pub body: String,
}

/// Processes listed in full before the dialog says "and N more".
pub const MAX_LISTED_USERS: usize = 8;

pub fn build(
    drive: &str,
    failure: &EjectFailure,
    users: &[DriveUser],
    note: Option<&str>,
) -> Report {
    let mut spoken = format!("eject failed: {}", failure.summary);
    match users.len() {
        0 => {}
        1 => spoken.push_str(&format!(". {} is using the drive", users[0].name)),
        n => spoken.push_str(&format!(
            ". {} programs are using the drive, including {}",
            n, users[0].name
        )),
    }

    let mut body = format!("Could not eject {}.\n{}\n", drive, failure.summary);
    if let Some(v) = &failure.veto {
        body.push_str(&format!("PnP veto: {}\n", v));
    }
    if let Some(n) = note {
        body.push_str(n);
        body.push('\n');
    }
    body.push('\n');
    if users.is_empty() {
        body.push_str(
            "No program with open files on the drive was found.\n\
             Something may hold the volume itself (an indexer, antivirus, or a \
             terminal whose current folder is on the drive). Close windows that \
             show the drive and try again.",
        );
    } else {
        body.push_str("Programs using the drive. Close them and try again:\n");
        for u in users.iter().take(MAX_LISTED_USERS) {
            body.push_str(&format!("\n• {} (pid {})", u.name, u.pid));
            if u.runs_from_drive {
                body.push_str(" — running from the drive");
            }
            body.push('\n');
            for p in &u.paths {
                body.push_str(&format!("    {}\n", p));
            }
            if u.total_paths > u.paths.len() {
                body.push_str(&format!(
                    "    … and {} more\n",
                    u.total_paths - u.paths.len()
                ));
            }
        }
        if users.len() > MAX_LISTED_USERS {
            body.push_str(&format!(
                "\n… and {} more programs\n",
                users.len() - MAX_LISTED_USERS
            ));
        }
    }
    Report { spoken, body }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure() -> EjectFailure {
        EjectFailure {
            summary: "D: the drive is in use — a program still has files open on it".into(),
            veto: Some("OutstandingOpen: USB\\VID_04E8&PID_61F5\\S6WPNS0X123".into()),
        }
    }

    fn user(name: &str, pid: u32, paths: &[&str], total: usize) -> DriveUser {
        DriveUser {
            pid,
            name: name.into(),
            runs_from_drive: false,
            paths: paths.iter().map(|s| s.to_string()).collect(),
            total_paths: total,
        }
    }

    #[test]
    fn names_the_program_and_its_files() {
        let users = [user("notepad.exe", 1234, &["D:\\notes.txt"], 1)];
        let r = build("D: (data)", &failure(), &users, None);
        assert!(
            r.spoken.contains("notepad.exe is using the drive"),
            "{}",
            r.spoken
        );
        assert!(
            r.body
                .contains("• notepad.exe (pid 1234)\n    D:\\notes.txt\n"),
            "{}",
            r.body
        );
        assert!(r.body.contains("PnP veto: OutstandingOpen"), "{}", r.body);
        assert!(!r.body.contains("more"), "{}", r.body);
    }

    #[test]
    fn overflow_is_counted_not_dropped() {
        let users = [user(
            "code.exe",
            7,
            &["D:\\a", "D:\\b", "D:\\c", "D:\\d"],
            12,
        )];
        let r = build("D:", &failure(), &users, None);
        assert!(r.body.contains("… and 8 more\n"), "{}", r.body);
    }

    #[test]
    fn several_programs_are_summarised_in_speech() {
        let users = [
            user("a.exe", 1, &["D:\\x"], 1),
            user("b.exe", 2, &["D:\\y"], 1),
        ];
        let r = build("D:", &failure(), &users, None);
        assert!(
            r.spoken
                .contains("2 programs are using the drive, including a.exe"),
            "{}",
            r.spoken
        );
    }

    #[test]
    fn no_users_found_still_explains_itself() {
        let r = build("D:", &failure(), &[], Some("Scanned as administrator."));
        assert!(r.body.contains("No program with open files"), "{}", r.body);
        assert!(r.body.contains("Scanned as administrator.\n"), "{}", r.body);
        assert!(!r.spoken.contains("using the drive"), "{}", r.spoken);
    }
}
