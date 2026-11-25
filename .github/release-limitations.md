## Known limitations

- The macOS backends are compile-tested only. MediaRemote and AppleScript runtime behavior is unverified on real hardware; run `lyrachi debug mediaremote` or `lyrachi debug applescript` if something looks off.
- The aarch64 Linux archive targets glibc 2.39 (the Ubuntu 24.04 baseline), so older ARM distributions need the crate build instead.
