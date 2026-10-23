# Original icon

The first InternetBlazer icon: a white download arrow on an orange-to-red gradient tile.
Replaced on 2026-10-23 by `design/app-icon.png`.

- `app/`: the Tauri icon set (copy back into `app/src-tauri/icons/`).
- `extension/`: the browser extension icons (copy back into `extension/icons/`).

The in-app logo was drawn in CSS rather than an image. To bring it back, replace the
`<img class="logo" ...>` tags in `app/src/App.svelte` and `app/src/lib/AboutDialog.svelte` with
`<span class="logo"><Icon name="download" size={16} /></span>` (size 30 in About) and use this style:

```css
.logo {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: 7px;
  color: #fff;
  background: linear-gradient(135deg, #ff8a3d, #e5322d);
}
```
