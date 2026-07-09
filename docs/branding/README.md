# Branding assets — regeneration pipeline

The README hero banner (`docs/loadout-hero.png`) is an HTML page rendered
headless, compositing real product captures. Nothing in it is mocked: the
studio window is a live screenshot and the terminal shows verbatim
`load claude` output. Regenerate it whenever the studio UI or the launch
output changes.

## Files

- `hero.html` — the banner compositor (1200×675 CSS @2x → 2400×1350 PNG).
  Fonts load from Google Fonts at render time (network needed). Brand icons
  are inlined from [simple-icons](https://simpleicons.org/).
- `capture-studio.ts` — Deno + CDP: boots headless Chromium, opens the studio
  bootstrap URL, clicks the **rust** loadout card, screenshots at 1180×956 @2x.
- `capture-workflows.ts` — same, but navigates Library → Workflows.
- `studio-rust-clean.png` — the processed capture `hero.html` embeds
  (cropped to 1500px, summary strip painted out, right-side chrome that the
  canvas edge would sever painted out — see step 3).

## Regenerating

1. **Capture** (each bootstrap token is single-use — restart `load studio`
   per capture):

   ```bash
   load studio --no-open --port 7788 --idle-timeout 5m &
   # copy the bootstrap URL it prints, then:
   deno run --allow-net --allow-run --allow-write capture-studio.ts "<bootstrap-url>" studio-rust.png
   ```

2. **Refresh the docs screenshots** (Pillow via uv):

   ```bash
   uv run --with pillow python3 -c "
   from PIL import Image
   Image.open('studio-rust.png').crop((0, 0, 2360, 1440)).save('../screenshots/loadouts.png')"
   # workflows: capture-workflows.ts, then crop to (0, 0, 2360, 1650)
   ```

3. **Rebuild the banner input.** Crop to 1500px tall, paint the app's own
   background over the summary strip, and paint out whatever the banner's
   right canvas edge would cut mid-word (in the current layout: the
   "No staged changes" nav text, the Preview button row, and the third
   fragment-chip column). Sample fill colors from the capture itself.
   The rects used last time live in the session that produced this file;
   re-derive them by rendering once and looking for severed text.

4. **Render + crop.** New-style headless Chrome's `--window-size` includes
   ~120px of browser chrome, so oversize and crop:

   ```bash
   chromium --headless=new --disable-gpu --screenshot=hero.png \
     --window-size=1200,795 --force-device-scale-factor=2 \
     --hide-scrollbars --virtual-time-budget=10000 "file://$PWD/hero.html"
   uv run --with pillow python3 -c "
   from PIL import Image
   Image.open('hero.png').crop((0, 0, 2400, 1350)).save('../loadout-hero.png')"
   ```

5. **Check before committing:** no text severed at any window or canvas
   edge; the terminal window fully closed; workflow card fully visible;
   `load claude` output still matches what the CLI actually prints.
