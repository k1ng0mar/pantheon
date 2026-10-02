# contrib/ - deployment helpers

Production-grade extras that do not belong in the binary: service
definitions, example configs, and similar.

## Service supervision

### systemd (Linux) - `pantheon-gateway.service`

A user-level unit for `pantheon gateway run` with `Restart=on-failure`,
restart backoff, a start-limit burst guard, and standard user-service
hardening (`NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict` with a
`ReadWritePaths` exception for the data dir).

Quick install:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/pantheon-gateway.service ~/.config/systemd/user/
# edit ExecStart to the real binary path (command -v pantheon)
systemctl --user daemon-reload
loginctl enable-linger "$USER"
systemctl --user enable --now pantheon-gateway.service
```

Or let Pantheon do it: `pantheon gateway start` installs the service for
the current user when none is installed yet (same idea, generated for
your paths) and ensures it is running - use that when you do not want to
hand-edit the file.

### Other process managers

- **macOS (launchd):** there is no launchd plist in this directory yet.
  The equivalent is a `~/Library/LaunchAgents/com.pantheon.gateway.plist`
  with `KeepAlive` and `RunAtLoad`, pointing at the same
  `pantheon gateway run` command. Contributions welcome.
- **Windows (Task Scheduler):** create a logon-triggered task running
  `pantheon.exe gateway run`, set to restart on failure.
- **Anything else (cron, supervisord, s6):** the gateway is a normal
  foreground process - run `pantheon gateway run` under whatever
  supervisor you already use. No forking, no pidfiles.

### What is deliberately out of scope

A Pantheon-specific watchdog daemon (a second process that polls
`/api/health` and restarts the gateway) is **out of scope** by design:
that is the process manager's job. systemd's `Restart=` above, launchd's
`KeepAlive`, and Task Scheduler's restart-on-failure all cover it. If you
want health-aware restarts beyond "the process died", poll
`GET /api/health/channels` from your own monitor and restart the service
when a channel stays `dead`.
