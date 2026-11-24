//! AppleScript sources for one-shot `osascript` polls.
//!
//! Each script targets one player and the caller only assembles a script for
//! players `pgrep` saw running, so terminology resolution never touches an
//! uninstalled app. `@FS@`/`@RS@` placeholders become the field/row
//! separators at assembly time; the separator bytes never appear in this
//! source file. Every tell block reports failures as an `error` row carrying
//! the Apple event number, which keeps TCC classification working even in
//! the combined script.

use crate::media::macos::parse::{FIELD_SEP, PlayerId, ROW_SEP};

/// Apple event timeout inside the script. The consent dialog blocks the
/// first event until the user answers, so the first wait must outlast a
/// human; the Rust-side watchdog enforces the shorter per-state budgets.
pub const SCRIPT_TIMEOUT_SECS: u32 = 120;

const MUSIC_BLOCK: &str = r#"
set musicRow to ""
try
	tell application "Music"
		set mst to player state as text
		if mst is "stopped" then
			set musicRow to "music@FS@stopped@FS@@FS@@FS@@FS@@FS@@FS@@FS@"
		else
			set mpos to ""
			try
				set mpos to (player position as integer) as text
			end try
			set mdur to ""
			set mtime to ""
			set mtitle to ""
			set martist to ""
			set malbum to ""
			set mid to ""
			try
				set t to current track
				try
					set mdur to ((duration of t) as integer) as text
				end try
				try
					set mtime to (time of t) as text
				end try
				set mtitle to (name of t) as text
				set martist to (artist of t) as text
				set mid to (persistent ID of t) as text
			end try
			try
				set malbum to (album of current track) as text
			end try
			set musicRow to "music@FS@" & mst & "@FS@" & mpos & "@FS@" & mdur & "@FS@" & mtime & "@FS@" & mtitle & "@FS@" & martist & "@FS@" & malbum & "@FS@" & mid
		end if
	end tell
on error errStr number errNum
	set musicRow to "music@FS@error@FS@" & (errNum as text) & "@FS@@FS@@FS@@FS@@FS@@FS@@FS@"
end try
"#;

const SPOTIFY_BLOCK: &str = r#"
set spotRow to ""
try
	tell application "Spotify"
		set sst to player state as text
		if sst is "stopped" then
			set spotRow to "spotify@FS@stopped@FS@@FS@@FS@@FS@@FS@@FS@@FS@"
		else
			set spos to ""
			try
				set spos to (player position as integer) as text
			end try
			set sdur to ""
			set stitle to ""
			set sartist to ""
			set salbum to ""
			set sid to ""
			try
				set t to current track
				try
					set sdur to ((duration of t) as integer) as text
				end try
				set stitle to (name of t) as text
				set sartist to (artist of t) as text
				set sid to (id of t) as text
			end try
			try
				set salbum to (album of current track) as text
			end try
			set spotRow to "spotify@FS@" & sst & "@FS@" & spos & "@FS@" & sdur & "@FS@" & stitle & "@FS@" & sartist & "@FS@" & salbum & "@FS@" & sid
		end if
	end tell
on error errStr number errNum
	set spotRow to "spotify@FS@error@FS@" & (errNum as text) & "@FS@@FS@@FS@@FS@@FS@@FS@@FS@"
end try
"#;

/// Assemble the script for the given players, in poll order. The caller only
/// passes players `pgrep` confirmed running, so the slice is never empty.
pub fn script_for(players: &[PlayerId]) -> String {
    debug_assert!(!players.is_empty());
    let mut script = format!("with timeout of {SCRIPT_TIMEOUT_SECS} seconds\nset out to \"\"\n");
    for player in players {
        script.push_str(match player {
            PlayerId::Music => MUSIC_BLOCK,
            PlayerId::Spotify => SPOTIFY_BLOCK,
        });
    }
    let joined = players
        .iter()
        .map(|player| match player {
            PlayerId::Music => "musicRow",
            PlayerId::Spotify => "spotRow",
        })
        .collect::<Vec<_>>()
        .join(&format!(" & \"{}\" & ", ROW_SEP));
    script.push_str("set out to ");
    script.push_str(&joined);
    script.push_str("\nreturn out\n");
    let field_sep = FIELD_SEP.to_string();
    let row_sep = ROW_SEP.to_string();
    script.replace("@FS@", &field_sep).replace("@RS@", &row_sep)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_player_script_contains_one_block() {
        let script = script_for(&[PlayerId::Music]);
        assert!(script.contains("tell application \"Music\""));
        assert!(!script.contains("tell application \"Spotify\""));
        assert!(script.starts_with("with timeout of 120 seconds"));
        assert!(script.contains("return out"));
    }

    #[test]
    fn combined_script_joins_rows_with_the_row_separator() {
        let script = script_for(&[PlayerId::Music, PlayerId::Spotify]);
        assert!(script.contains("tell application \"Music\""));
        assert!(script.contains("tell application \"Spotify\""));
        assert!(script.contains(&format!("musicRow & \"{}\" & spotRow", ROW_SEP)));
    }

    #[test]
    fn placeholders_are_fully_resolved() {
        for players in [
            &[PlayerId::Music][..],
            &[PlayerId::Spotify][..],
            &[PlayerId::Music, PlayerId::Spotify][..],
        ] {
            let script = script_for(players);
            assert!(!script.contains("@FS@"));
            assert!(!script.contains("@RS@"));
            assert!(script.contains(FIELD_SEP));
        }
    }

    #[test]
    fn stopped_rows_carry_nine_fields() {
        assert!(MUSIC_BLOCK.contains("music@FS@stopped@FS@@FS@@FS@@FS@@FS@@FS@@FS@"));
        let resolved = script_for(&[PlayerId::Music]);
        let line = resolved
            .lines()
            .find(|line| line.contains(&format!("music{FIELD_SEP}stopped")))
            .expect("stopped row");
        assert_eq!(line.split(FIELD_SEP).count(), 9);
    }
}
