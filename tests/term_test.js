// The browser terminal's parser, tested without a browser.
//
// A VT emulator is the kind of code where "it looked right" is not evidence:
// the pending-wrap rule, a scroll region and an alternate screen all go wrong
// quietly and only on somebody else's TUI. So the parser is driven here with
// the sequences agent CLIs actually emit, and the grid is read back.
//
// Run by tests/run-all.sh. Node is already needed for nothing else here, so
// it is skipped rather than failed if absent.
"use strict";

// --------------------------------------------------------------- the DOM ----
// term.js touches very little of it: an element with classList, children,
// appendChild and a bounding box. Stubbed, rather than pulling in a headless
// browser to test a state machine.
function el() {
  const e = {
    children: [],
    style: {},
    classList: { add() {}, remove() {} },
    setAttribute() {}, removeAttribute() {},
    addEventListener() {},
    appendChild(c) { this.children.push(c); c.parentNode = this; return c; },
    remove() {
      if (!this.parentNode) return;
      const i = this.parentNode.children.indexOf(this);
      if (i >= 0) this.parentNode.children.splice(i, 1);
    },
    getBoundingClientRect() { return { width: 800, height: 400 }; },
    set textContent(v) { this._text = v; },
    get textContent() { return this._text || ""; },
    set innerHTML(v) { this._html = v; },
    get innerHTML() { return this._html || ""; },
    get lastChild() { return this.children[this.children.length - 1]; },
  };
  return e;
}
global.document = { createElement: () => el() };
global.window = {};
global.TextDecoder = require("util").TextDecoder;
global.TextEncoder = require("util").TextEncoder;

const fs = require("fs");
const path = require("path");
const src = fs.readFileSync(
  path.join(__dirname, "..", "crates", "hub", "static", "term.js"), "utf8");
// The file ends by publishing on `window`; evaluating it here does the same.
new Function(src)();
const NexTerm = global.window.NexTerm;

// --------------------------------------------------------------- harness ----
let fails = 0;
function check(label, cond, detail) {
  console.log(`  ${cond ? "PASS" : "FAIL"}  ${label}` +
              (!cond && detail !== undefined ? `  -- ${detail}` : ""));
  if (!cond) fails++;
}

function mk(cols = 20, rows = 6) {
  const t = new NexTerm(el());
  t.resize(cols, rows);
  return t;
}

/// What is on a row, as text.
function row(t, y) {
  return t.screen.lines[y].map((c) => c.c).join("").replace(/\s+$/, "");
}

function screen(t) {
  return t.screen.lines.map((_, y) => row(t, y));
}

// ----------------------------------------------------------------- tests ----
console.log("=== text, wrapping and the pending-wrap rule ===");
{
  const t = mk(10, 4);
  t.write("hello");
  check("plain text lands on the first row", row(t, 0) === "hello", row(t, 0));

  // A write that fills the last column must NOT move to the next row until
  // one more character arrives. Getting this wrong puts a blank line into
  // every full-width box a TUI draws.
  const u = mk(5, 3);
  u.write("abcde");
  check("a full row leaves the cursor on it", u.cy === 0 && u.cx === 4, `${u.cx},${u.cy}`);
  check("and marks the wrap as pending", u.wrapNext === true);
  u.write("f");
  check("the next character wraps", u.cy === 1 && row(u, 1) === "f", `${u.cy} ${row(u, 1)}`);
  check("without eating the full row", row(u, 0) === "abcde", row(u, 0));
}

console.log("\n=== the cursor, and erasing ===");
{
  const t = mk(10, 4);
  t.write("\x1b[2;3Hxy");
  check("CSI H positions from 1, not 0", row(t, 1) === "  xy", JSON.stringify(row(t, 1)));
  t.write("\x1b[1;1H\x1b[K");
  check("EL 0 clears to the end of the line", row(t, 0) === "", JSON.stringify(row(t, 0)));
  t.write("\x1b[2;1Hab\x1b[2;2H\x1b[1K");
  check("EL 1 clears to the cursor inclusive", row(t, 1).startsWith("  "), JSON.stringify(row(t, 1)));
  t.write("\x1b[2Jdone");
  check("ED 2 clears everything", screen(t).filter((r) => r !== "").length === 1, JSON.stringify(screen(t)));
}

console.log("\n=== scroll region: how a CLI redraws above a fixed prompt ===");
{
  const t = mk(8, 5);
  t.write("1\r\n2\r\n3\r\n4\r\n5");
  check("five rows of history", screen(t).join("|") === "1|2|3|4|5", screen(t).join("|"));
  // Confine scrolling to the top three rows, then push past the bottom of it.
  t.write("\x1b[1;3r\x1b[3;1H\r\nnew");
  check("only the region moved", row(t, 3) === "4" && row(t, 4) === "5",
        JSON.stringify(screen(t)));
  check("and the region scrolled", row(t, 2) === "new", JSON.stringify(screen(t)));
  // Reverse index at the top of the region scrolls it back down.
  t.write("\x1b[1;1H\x1bM");
  check("reverse index scrolls the region down", row(t, 1) === "2", JSON.stringify(screen(t)));
}

console.log("\n=== insert and delete, which ink uses constantly ===");
{
  const t = mk(10, 3);
  t.write("abcdef\x1b[1;3H\x1b[2P");
  check("DCH deletes characters and pulls the rest left",
        row(t, 0) === "abef", JSON.stringify(row(t, 0)));
  t.write("\x1b[1;3H\x1b[2@");
  check("ICH opens a gap", row(t, 0) === "ab  ef", JSON.stringify(row(t, 0)));
  const u = mk(6, 4);
  u.write("1\r\n2\r\n3");
  u.write("\x1b[1;1H\x1b[L");
  check("IL pushes the lines down", row(u, 1) === "1" && row(u, 2) === "2",
        JSON.stringify(screen(u)));
  u.write("\x1b[1;1H\x1b[M");
  check("DL pulls them back", row(u, 0) === "1", JSON.stringify(screen(u)));
}

console.log("\n=== the alternate screen ===");
{
  const t = mk(8, 3);
  t.write("keep this");
  t.write("\x1b[?1049h");
  check("the alt screen starts clean", screen(t).join("") === "", JSON.stringify(screen(t)));
  t.write("full screen app");
  t.write("\x1b[?1049l");
  check("and leaving it gives back what was underneath",
        row(t, 0) === "keep thi" || row(t, 0).startsWith("keep"), JSON.stringify(screen(t)));
}

console.log("\n=== colour ===");
{
  // Wide enough that nothing here wraps: a row that scrolls under the test
  // is the test's fault, and it cost ten minutes to say so.
  const t = mk(40, 4);
  t.write("\x1b[31mred\x1b[0m plain");
  const cells = t.screen.lines[0];
  check("SGR 31 sets a foreground from the theme's variables",
        cells[0].f === "var(--a1)", cells[0].f);
  check("SGR 0 puts it back", cells[4].f === null, String(cells[4].f));
  t.write("\x1b[2;1H\x1b[38;2;10;20;30mtruecolor");
  check("38;2 is truecolor", t.screen.lines[1][0].f === "rgb(10,20,30)", t.screen.lines[1][0].f);
  t.write("\x1b[3;1H\x1b[38;5;196mcube");
  check("38;5 is the 256 cube", t.screen.lines[2][0].f === "rgb(255,0,0)",
        String(t.screen.lines[2][0].f));
  t.write("\x1b[4;1H\x1b[0m\x1b[1m\x1b[4mboth");
  const bits = t.screen.lines[3][0].a;
  check("bold and underline are attributes, not colours",
        (bits & 1) === 1 && (bits & 8) === 8, String(bits));
  t.write("\x1b[22m\x1b[24mx");
  check("and they can be turned off again", t.screen.lines[3][4].a === 0,
        String(t.screen.lines[3][4].a));
}

console.log("\n=== what it answers, because a program that asks will wait ===");
{
  const t = mk(10, 3);
  const sent = [];
  t.onInput((b) => sent.push(Buffer.from(b).toString("latin1")));
  t.write("\x1b[2;4H\x1b[6n");
  check("a cursor-position query is answered with where it is",
        sent.join("") === "\x1b[2;4R", JSON.stringify(sent.join("")));
  sent.length = 0;
  t.write("\x1b[c");
  check("and a device-attributes query gets an answer at all",
        sent.join("").startsWith("\x1b[?"), JSON.stringify(sent.join("")));
}

console.log("\n=== keys ===");
{
  const t = mk(10, 3);
  const sent = [];
  t.onInput((b) => sent.push(Buffer.from(b).toString("latin1")));
  const key = (k, mods = {}) => t.onKey(Object.assign(
    { key: k, preventDefault() {}, ctrlKey: false, altKey: false, shiftKey: false, metaKey: false },
    mods));
  key("a");
  key("Enter");
  key("ArrowUp");
  key("c", { ctrlKey: true });
  key("Tab", { shiftKey: true });
  check("a letter, Enter, an arrow, Ctrl-C and Shift-Tab all send the right bytes",
        sent.join("") === "a\r\x1b[A\x03\x1b[Z", JSON.stringify(sent.join("")));
  sent.length = 0;
  // A read-only attach is a headless agent: watchable, not driveable.
  t.readOnly = true;
  key("x");
  check("a read-only terminal sends nothing", sent.length === 0, JSON.stringify(sent));
}

console.log("\n=== rendering ===");
{
  const t = mk(6, 2);
  t.write("a<b&c");
  const html = t.rowHtml(0);
  check("markup in the output is escaped, not interpreted",
        html.includes("a&lt;b&amp;c"), html);
  t.write("\x1b[?25l");
  const hidden = t.rowHtml(0);
  t.write("\x1b[?25h");
  const shown = t.rowHtml(0);
  check("the cursor is drawn only when the program wants it",
        hidden !== shown, `${hidden} / ${shown}`);
}

console.log();
if (fails) {
  console.log(`FAILED (${fails})`);
  process.exit(1);
}
console.log("ALL BROWSER TERMINAL CHECKS PASSED");
