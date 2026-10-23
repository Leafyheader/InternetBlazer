# Design

- `app-icon.png`: the source of every app and extension icon. To regenerate them, run
  `npx tauri icon ../design/app-icon.png` in `app/` (then delete the `android` and `ios` folders
  it adds), and resize it to 32, 64 and 128 pixels for `extension/icons/` and to 128 for
  `app/src/lib/logo.png`.
- `original-icon/`: the first icon, kept to switch back to.
