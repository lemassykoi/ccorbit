# Changelog

## Unreleased

- **Jump to a session** (`--allow-jump`, then `o`): brings forward the terminal tab where the
  pointed session runs. In Warp the exact tab (its focus link); elsewhere on macOS the terminal
  app. Off by default: it runs `open` and reads the session's process environment.
- **The hologram runs at 30 frames per second**, sharp at any size, for about 3% CPU; `--fps`
  changes it (`--fps 60` costs about 5%).
- **Quieter notices:** failed shell commands no longer raise one each (the feed shows them, and
  repeated failures raise a warning); the same notice again is counted (`×4`) instead of stacking;
  notices slide in whole.
- Hidden `--jump-check` and `--bench-hud` options for diagnostics.

## 0.1.0 (2026-10-17)

First release.

- Wall of session cards with skyline and signal feed; session tree with vitals and the live
  agents of running workflows; 3D hologram HUD with a flat view and the director; code city.
- Readings: brainwave of thinking, writing and tool time; prompt cache hit ratio; API-equivalent
  price; context against the compaction ceiling; slow and looping calls; files changed by two
  sessions at once; workflow runs raced against earlier ones.
- `--demo` mode with made-up sessions.
- Install by script, Homebrew (`pcangemi/tap`) or Cargo; builds for macOS and Linux, arm and x86.
