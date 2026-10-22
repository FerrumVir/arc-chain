# ARC desktop: what the app sends about itself

## Anonymous error reports ("Node Care")

The desktop app can send a short report when something goes wrong with your
node, so problems get fixed without anyone posting screenshots. It is **on by
default** and can be turned off at any time in **Settings → Error reports →
"Send anonymous error reports to help fix problems"**. Turning it off takes
effect at once: anything waiting to be sent is deleted and nothing more goes
out.

The first time ARC opens with this feature, it shows a one-time notice. No
report leaves the computer until that notice has been shown.

**Settings → Error reports → See what's sent** shows the exact JSON of the
last five payloads and whether each one was sent.

### When a report is sent

| Event | When |
|---|---|
| `start_failed` | A Start or Restart is refused, the app's own start at launch fails, or the start fence blocks starts. |
| `crash` | arc-node exits unexpectedly. |
| `stuck` | The node has been starting, or the last session has been closing, for 10 minutes. |
| `update_failed` | An update check, download, install or relaunch fails. |
| `recovered` | The node is running again after one of the problems above. |

### What a report contains (schema `arc.node-care.report.v1`)

| Field | Example | Notes |
|---|---|---|
| `schema` | `arc.node-care.report.v1` | |
| `install_id` | `3f9c…` (32 hex digits) | Random, made on this computer, stored in the app-data folder. Not derived from your wallet or hardware. **Settings → Reset install ID** replaces it. |
| `event` | `start_failed` | One of the five events above. |
| `app_version` | `0.8.14` | |
| `node_version` | `0.8.14` | The arc-node version, when known. |
| `os` / `arch` | `macos` / `aarch64` | |
| `stage` | `start` | Which step failed, for example `start`, `auto-start`, `start-fence`, `running`, `update-check`. |
| `error_code` | `reconciling` | A short category, for example `blocked`, `needs-repair`, `exited-unexpectedly`. |
| `message` | `Couldn't start arc-node: …` | The error text, cleaned (below) and cut to 2 KB. |
| `uptime_s` | `3600` | How long the node had been running, when known. |
| `occurred_at` | `2026-10-22T12:00:00Z` | UTC, to the second. |

Before it is sent, `message` is cleaned on your computer:

- your home folder, and any other user folder in a path, becomes `~`;
- email addresses, IPv4 and IPv6 addresses, hostnames in URLs, `0x` hex of 40
  or more digits, plain hex of 32 or more digits and long key-like strings
  become `[redacted]`.

### The daily heartbeat (schema `arc.node-care.heartbeat.v1`)

At most once every 24 hours while ARC is open and the node is running,
starting or failing, it sends `schema`, `install_id`, `app_version`,
`node_version`, `os` and `status` (`earning`, `syncing`, `starting` or
`error`). This only counts how many nodes are working.

### What is never sent

Wallet addresses, keys, the recovery phrase, peer lists, IP addresses, file
contents, logs, prompts, or anything else not in the tables above. The app
refuses to send any field outside those lists.

### Delivery

Reports go by HTTPS POST to the address set when the app is built
(`ARC_NODE_CARE_ENDPOINT`, paths `/v1/report` and `/v1/heartbeat`), with a
5-second timeout. A build without that address sends nothing. Failed sends
wait in a small queue (at most 50 items, dropped after 7 days) and are retried
later. The same problem is reported at most once an hour, and no more than 20
distinct reports a day. Reporting never delays or blocks the node starting,
and its failures are silent.

Your network's address is visible to the receiving server when any HTTPS
request arrives, as with any connection; it is not part of the report.
