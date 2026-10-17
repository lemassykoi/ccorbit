# Looking at screens

A screen is the product: check how it looks, not only that it compiles.

## The golden screens

`cargo test` draws every screen of the demo (wall, session, 3D and flat hologram, code city, help)
at 120×36 and 170×50 at a frozen moment, and compares them with `tests/golden/*.txt`. When a test
says a screen changed:

1. Read the lines it prints: is that the change you meant?
2. If it is, run `UPDATE_GOLDEN=1 cargo test`, then read `git diff tests/golden` line by line:
   nothing else should have moved.
3. Say in the commit why the screen changed.

The golden copies are text: they show layout and words, not colors. For colors, look at the
screen itself.

## Seeing a screen without a terminal

```sh
cargo run --release -- --demo --snapshot 170x50                 # every view as text
COLORTERM=truecolor cargo run --release -- --demo --snapshot-html 170x50 /tmp/shots   # colored HTML
```

The HTML frames draw Braille as its raised dots only, like a terminal does. To get PNGs, open them
in a headless browser with a throwaway profile, for example:

```sh
chrome --headless=new --hide-scrollbars --user-data-dir=/tmp/ccorbit-chrome \
  --window-size=1530,900 --screenshot=/tmp/shots/wall.png file:///tmp/shots/wall.html
```

## Readability rules

- Text never sits on dots: labels in the hologram have a plate of background under them.
- Nothing is cut without an ellipsis (`…`); `fit()` in `src/ui.rs` does it for one-line rows.
- Every screen draws from 30×10 up without a panic (tested), and makes sense from 80×24.
- A session keeps its identity color everywhere: card, tower, feed, planet, district.
- Animations explain something (work in progress, traffic, a new node); decoration stays faint.
- Look at both sizes (120×36 and 170×50) and at the demo (`ccorbit --demo`), which has every kind
  of event.
