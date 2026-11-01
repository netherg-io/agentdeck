# COSMIC panel applet

Native COSMIC 1.8 frontend for the `agentdeck-daemon.service` usage state. Displays remaining quota percentages or balances on the panel and all quota windows, reset times and errors in its popup. Account email addresses are masked; click an account header to reveal one until the popup closes. The popup anchors to the actual panel button bounds, has a 480 px height cap, and uses a 4 px scroll thumb within an 8 px track. `~` marks stale data. No additional network requests or credentials.

Choose which providers the panel and popup show in agentdeck under **Quotas → Panel**.
The choice is saved in `~/.config/agentdeck/ai-usage-visibility.json` (respecting
`AGENTDECK_CONFIG_DIR` and `XDG_CONFIG_HOME`) and the applet re-reads it every five
seconds; backend polling and routing quotas are unaffected.

From the workspace root:

```sh
unset APPIMAGE APPDIR LD_LIBRARY_PATH
cargo test -p agentdeck-applet --locked --release --jobs 2
cargo build -p agentdeck-applet --locked --release --jobs 2
install -Dm755 target/release/agentdeck-applet ~/.local/bin/agentdeck-applet
install -Dm644 crates/agentdeck-applet/io.github.netherguy4.AiUsage.desktop ~/.local/share/applications/io.github.netherguy4.AiUsage.desktop
```

Add **agentdeck** in COSMIC panel settings. Keep `agentdeck-daemon.service` running. The applet reads `$XDG_RUNTIME_DIR/agentdeck/usage/state.json` every five seconds (falling back to the old `$XDG_RUNTIME_DIR/ai-usage/state.json` during migration) and shares the backend's schema and provider icons. Rust comes from a local `rustup override`, the C toolchain is the layered system `gcc`; the base OS needs no compiler installation.

`Cargo.lock` pins libcosmic and its configuration crate together to COSMIC 1.8 (`d4d71fd53e5ed6bd3a430089114dffa2da3cd498`).
