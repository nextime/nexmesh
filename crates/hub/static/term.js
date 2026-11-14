// A terminal, in the browser, for watching and driving an agent.
//
// Served by the hub like everything else here: no CDN, no npm, nothing to
// fetch from the internet, because a fleet hub may sit on a network with no
// route to one. xterm.js would be the obvious dependency and is a fine
// library; it is 280KB of somebody else's code in a repository that vendors
// none, and what a TUI agent actually emits is a knowable subset. So this is
// that subset, deliberately: enough VT to render an agent's own interface
// faithfully, and nothing for the sake of completeness.
//
// It lives in its own file rather than inline in the page for the same reason
// the service worker does -- a parser is a parser and the page is a page.
//
// What is implemented, and why each one is here rather than skipped:
//   - the pending-wrap rule, which every box-drawing TUI depends on;
//   - a scroll region with index and reverse index, which is how a CLI that
//     prints above a fixed prompt redraws itself;
//   - insert/delete lines and characters, which ink uses constantly;
//   - the alternate screen, so a full-screen program does not destroy the
//     scrollback behind it;
//   - SGR including 256-colour and truecolor, because agent CLIs are colourful
//     and a monochrome transcript is much harder to read;
//   - cursor-position and device-attribute replies, because a program that
//     asks and is never answered waits.
//
// Everything below is inside one function on purpose: a page and a library
// that are both classic scripts share one global scope, and the first thing
// this file did when it met the dashboard was collide with its `esc`. Only
// `window.NexTerm` leaves here.
(function () {
"use strict";

const ATTR = { BOLD: 1, DIM: 2, ITALIC: 4, UNDER: 8, INVERSE: 16, STRIKE: 32, HIDDEN: 64 };

// The sixteen ANSI colours come from CSS variables, so a theme can restyle a
// terminal without this file knowing anything about themes.
const ANSI_VAR = (i) => `var(--a${i})`;

// The 256-colour cube, worked out rather than tabulated.
function xterm256(i) {
  if (i < 16) return ANSI_VAR(i);
  if (i < 232) {
    const n = i - 16;
    const q = [0, 95, 135, 175, 215, 255];
    return `rgb(${q[Math.floor(n / 36) % 6]},${q[Math.floor(n / 6) % 6]},${q[n % 6]})`;
  }
  const v = 8 + (i - 232) * 10;
  return `rgb(${v},${v},${v})`;
}

function blank() {
  return { c: " ", f: null, b: null, a: 0 };
}

class Screen {
  constructor(cols, rows) {
    this.cols = cols;
    this.rows = rows;
    this.lines = [];
    for (let y = 0; y < rows; y++) this.lines.push(this.newLine());
  }
  newLine() {
    const l = new Array(this.cols);
    for (let x = 0; x < this.cols; x++) l[x] = blank();
    return l;
  }
}

class NexTerm {
  constructor(host) {
    this.host = host;
    this.cols = 80;
    this.rows = 24;
    this.screen = new Screen(this.cols, this.rows);
    this.alt = null;               // the normal screen, while the alt is up
    this.cx = 0;
    this.cy = 0;
    this.saved = null;
    this.top = 0;                  // scroll region, inclusive
    this.bot = this.rows - 1;
    this.wrapNext = false;         // the pending-wrap rule
    this.autowrap = true;
    this.cursorOn = true;
    this.bracketed = false;
    this.attr = 0;
    this.fg = null;
    this.bg = null;
    this.state = "ground";
    this.params = "";
    this.inter = "";
    this.osc = "";
    this.dirty = new Set();
    this.decoder = new TextDecoder("utf-8", { fatal: false });
    this.inputCb = null;
    this.resizeCb = null;
    this.titleCb = null;
    this.readOnly = false;

    host.classList.add("nexterm");
    host.setAttribute("tabindex", "0");
    this.out = document.createElement("div");
    this.out.className = "ntgrid";
    host.appendChild(this.out);
    // A hidden textarea is how a phone is persuaded to show its keyboard, and
    // how a paste arrives as one event rather than a hundred keystrokes.
    this.ta = document.createElement("textarea");
    this.ta.className = "ntinput";
    this.ta.setAttribute("autocapitalize", "off");
    this.ta.setAttribute("autocomplete", "off");
    this.ta.setAttribute("autocorrect", "off");
    this.ta.setAttribute("spellcheck", "false");
    host.appendChild(this.ta);

    this.ta.addEventListener("keydown", (e) => this.onKey(e));
    this.ta.addEventListener("paste", (e) => this.onPaste(e));
    host.addEventListener("mousedown", (e) => {
      // Let a click-drag select text; only a plain click takes the keyboard.
      if (e.detail > 1) return;
      setTimeout(() => this.focus(), 0);
    });
    this.measure();
  }

  onInput(cb) { this.inputCb = cb; }
  onResize(cb) { this.resizeCb = cb; }
  onTitle(cb) { this.titleCb = cb; }
  focus() { try { this.ta.focus({ preventScroll: true }); } catch (e) {} }

  send(bytes) {
    if (this.readOnly || !this.inputCb) return;
    this.inputCb(typeof bytes === "string" ? new TextEncoder().encode(bytes) : bytes);
  }

  // ---------------------------------------------------------------- size ----

  /// One cell, measured from the real font rather than assumed: a terminal
  /// whose idea of a column is a pixel out draws a box that does not close.
  measure() {
    const probe = document.createElement("span");
    probe.className = "ntprobe";
    probe.textContent = "M".repeat(10);
    this.host.appendChild(probe);
    const r = probe.getBoundingClientRect();
    this.cellW = r.width / 10 || 8;
    this.cellH = r.height || 17;
    probe.remove();
  }

  fit() {
    this.measure();
    const r = this.host.getBoundingClientRect();
    const cols = Math.max(20, Math.floor((r.width - 8) / this.cellW));
    const rows = Math.max(6, Math.floor((r.height - 6) / this.cellH));
    if (cols === this.cols && rows === this.rows) return;
    this.resize(cols, rows);
    if (this.resizeCb) this.resizeCb(this.cols, this.rows);
  }

  /// Reflow is deliberately simple: keep what fits, anchored at the bottom.
  /// A real reflow rewraps history, which needs the history a viewer of a
  /// live terminal does not have -- the node replays bytes, and the program
  /// redraws itself on resize anyway.
  resize(cols, rows) {
    const old = this.screen.lines;
    const keep = Math.min(rows, this.rows);
    const s = new Screen(cols, rows);
    for (let i = 0; i < keep; i++) {
      const from = old[this.rows - keep + i];
      const to = s.lines[rows - keep + i];
      for (let x = 0; x < Math.min(cols, this.cols); x++) to[x] = from[x];
    }
    this.cols = cols;
    this.rows = rows;
    this.screen = s;
    this.top = 0;
    this.bot = rows - 1;
    this.cx = Math.min(this.cx, cols - 1);
    this.cy = Math.min(this.cy, rows - 1);
    this.alt = null;
    this.allDirty();
    this.render();
  }

  allDirty() {
    for (let y = 0; y < this.rows; y++) this.dirty.add(y);
  }

  // -------------------------------------------------------------- parsing ---

  write(data) {
    const text = typeof data === "string"
      ? data
      : this.decoder.decode(data instanceof Uint8Array ? data : new Uint8Array(data), { stream: true });
    for (const ch of text) this.step(ch);
    this.render();
  }

  step(ch) {
    const code = ch.codePointAt(0);
    switch (this.state) {
      case "ground":
        if (ch === "\x1b") { this.state = "esc"; this.params = ""; this.inter = ""; return; }
        if (code < 0x20 || code === 0x7f) return this.control(ch);
        return this.put(ch);
      case "esc":
        if (ch === "[") { this.state = "csi"; this.params = ""; this.inter = ""; return; }
        if (ch === "]") { this.state = "osc"; this.osc = ""; return; }
        if (ch === "P" || ch === "^" || ch === "_") { this.state = "dcs"; return; }
        return this.escape(ch);
      case "csi":
        if (ch >= "0" && ch <= "9" || ch === ";" || ch === ":" || ch === "?" || ch === ">" || ch === "<" || ch === "=") {
          this.params += ch;
          return;
        }
        if (ch === " " || ch === "!" || ch === "\"" || ch === "'" || ch === "$" || ch === "#") {
          this.inter += ch;
          return;
        }
        this.state = "ground";
        return this.csi(ch);
      case "osc":
        // Ends at BEL or at ST (ESC \). The title is the only one worth
        // keeping; the rest are swallowed rather than drawn as text.
        if (ch === "\x07") { this.state = "ground"; return this.oscDone(); }
        if (ch === "\x1b") { this.state = "oscesc"; return; }
        this.osc += ch;
        return;
      case "oscesc":
        this.state = "ground";
        return this.oscDone();
      case "dcs":
        // Device control strings: nothing here answers one, so they are
        // consumed to their terminator and ignored.
        if (ch === "\x1b") this.state = "dcsesc";
        return;
      case "dcsesc":
        this.state = "ground";
        return;
    }
  }

  oscDone() {
    const m = /^(\d+);([\s\S]*)$/.exec(this.osc);
    if (m && (m[1] === "0" || m[1] === "2") && this.titleCb) this.titleCb(m[2]);
    this.osc = "";
  }

  control(ch) {
    switch (ch) {
      case "\n": case "\x0b": case "\x0c":
        this.lineFeed();
        return;
      case "\r":
        this.cx = 0; this.wrapNext = false;
        return;
      case "\b":
        this.cx = Math.max(0, this.cx - 1); this.wrapNext = false;
        return;
      case "\t": {
        const next = Math.min(this.cols - 1, (Math.floor(this.cx / 8) + 1) * 8);
        this.cx = next; this.wrapNext = false;
        return;
      }
      case "\x07":
        return;                       // a bell in a web page is nobody's friend
      default:
        return;
    }
  }

  escape(ch) {
    this.state = "ground";
    switch (ch) {
      case "7": this.saveCursor(); return;
      case "8": this.restoreCursor(); return;
      case "D": this.lineFeed(); return;
      case "E": this.cx = 0; this.lineFeed(); return;
      case "M": this.reverseIndex(); return;
      case "c": this.reset(); return;
      // Charset selection: consume the byte that follows and carry on. This
      // terminal is UTF-8 and nothing else.
      case "(": case ")": case "*": case "+": this.state = "charset"; return;
      default: return;
    }
  }

  csi(final) {
    const priv = this.params.startsWith("?");
    const raw = priv ? this.params.slice(1) : this.params;
    const ps = raw.split(";").map((x) => (x === "" ? null : parseInt(x, 10) || 0));
    const p = (i, d) => (ps[i] === null || ps[i] === undefined ? d : ps[i]);

    switch (final) {
      case "A": this.cy = Math.max(this.top, this.cy - p(0, 1)); this.wrapNext = false; return;
      case "B": this.cy = Math.min(this.bot, this.cy + p(0, 1)); this.wrapNext = false; return;
      case "C": this.cx = Math.min(this.cols - 1, this.cx + p(0, 1)); this.wrapNext = false; return;
      case "D": this.cx = Math.max(0, this.cx - p(0, 1)); this.wrapNext = false; return;
      case "E": this.cx = 0; this.cy = Math.min(this.bot, this.cy + p(0, 1)); return;
      case "F": this.cx = 0; this.cy = Math.max(this.top, this.cy - p(0, 1)); return;
      case "G": case "`": this.cx = Math.min(this.cols - 1, Math.max(0, p(0, 1) - 1)); this.wrapNext = false; return;
      case "d": this.cy = Math.min(this.rows - 1, Math.max(0, p(0, 1) - 1)); this.wrapNext = false; return;
      case "H": case "f":
        this.cy = Math.min(this.rows - 1, Math.max(0, p(0, 1) - 1));
        this.cx = Math.min(this.cols - 1, Math.max(0, p(1, 1) - 1));
        this.wrapNext = false;
        return;
      case "J": return this.eraseDisplay(p(0, 0));
      case "K": return this.eraseLine(p(0, 0));
      case "L": return this.insertLines(p(0, 1));
      case "M": return this.deleteLines(p(0, 1));
      case "P": return this.deleteChars(p(0, 1));
      case "@": return this.insertChars(p(0, 1));
      case "X": return this.eraseChars(p(0, 1));
      case "S": return this.scrollUp(p(0, 1));
      case "T": return this.scrollDown(p(0, 1));
      case "m": return this.sgr(raw);
      case "r":
        this.top = Math.max(0, p(0, 1) - 1);
        this.bot = Math.min(this.rows - 1, p(1, this.rows) - 1);
        if (this.top >= this.bot) { this.top = 0; this.bot = this.rows - 1; }
        this.cx = 0; this.cy = this.top;
        return;
      case "s": this.saveCursor(); return;
      case "u": this.restoreCursor(); return;
      case "h": return this.mode(priv, ps, true);
      case "l": return this.mode(priv, ps, false);
      case "n":
        // A program that asks where the cursus is and is never told waits for
        // ever, which looks exactly like a hung agent.
        if (p(0, 0) === 6) this.send(`\x1b[${this.cy + 1};${this.cx + 1}R`);
        if (p(0, 0) === 5) this.send("\x1b[0n");
        return;
      case "c":
        this.send("\x1b[?62;c");      // "a VT220, with no options"
        return;
      default:
        return;
    }
  }

  mode(priv, ps, on) {
    if (!priv) return;
    for (const n of ps) {
      switch (n) {
        case 7: this.autowrap = on; break;
        case 25: this.cursorOn = on; this.dirty.add(this.cy); break;
        case 2004: this.bracketed = on; break;
        case 1049: case 1047: case 47:
          // The alternate screen. A full-screen program takes a clean one and
          // gives back what was underneath, which is why a TUI does not eat
          // your scrollback.
          if (on && !this.alt) {
            this.saveCursor();
            this.alt = this.screen;
            this.screen = new Screen(this.cols, this.rows);
            this.cx = 0; this.cy = 0;
          } else if (!on && this.alt) {
            this.screen = this.alt;
            this.alt = null;
            this.restoreCursor();
          }
          this.allDirty();
          break;
        default: break;
      }
    }
  }

  sgr(raw) {
    if (raw === "" || raw === "0") { this.attr = 0; this.fg = null; this.bg = null; return; }
    const ps = raw.split(";").map((x) => parseInt(x, 10) || 0);
    for (let i = 0; i < ps.length; i++) {
      const n = ps[i];
      if (n === 0) { this.attr = 0; this.fg = null; this.bg = null; }
      else if (n === 1) this.attr |= ATTR.BOLD;
      else if (n === 2) this.attr |= ATTR.DIM;
      else if (n === 3) this.attr |= ATTR.ITALIC;
      else if (n === 4) this.attr |= ATTR.UNDER;
      else if (n === 7) this.attr |= ATTR.INVERSE;
      else if (n === 8) this.attr |= ATTR.HIDDEN;
      else if (n === 9) this.attr |= ATTR.STRIKE;
      else if (n === 21 || n === 22) this.attr &= ~(ATTR.BOLD | ATTR.DIM);
      else if (n === 23) this.attr &= ~ATTR.ITALIC;
      else if (n === 24) this.attr &= ~ATTR.UNDER;
      else if (n === 27) this.attr &= ~ATTR.INVERSE;
      else if (n === 28) this.attr &= ~ATTR.HIDDEN;
      else if (n === 29) this.attr &= ~ATTR.STRIKE;
      else if (n >= 30 && n <= 37) this.fg = ANSI_VAR(n - 30);
      else if (n === 39) this.fg = null;
      else if (n >= 40 && n <= 47) this.bg = ANSI_VAR(n - 40);
      else if (n === 49) this.bg = null;
      else if (n >= 90 && n <= 97) this.fg = ANSI_VAR(n - 90 + 8);
      else if (n >= 100 && n <= 107) this.bg = ANSI_VAR(n - 100 + 8);
      else if (n === 38 || n === 48) {
        // 5;n is the cube, 2;r;g;b is truecolor. Both forms appear in the
        // wild from the same program.
        const kind = ps[i + 1];
        let colour = null;
        if (kind === 5) { colour = xterm256(ps[i + 2] || 0); i += 2; }
        else if (kind === 2) {
          colour = `rgb(${ps[i + 2] || 0},${ps[i + 3] || 0},${ps[i + 4] || 0})`;
          i += 4;
        } else { i += 1; }
        if (n === 38) this.fg = colour; else this.bg = colour;
      }
    }
  }

  // ------------------------------------------------------------ the grid ----

  cell() {
    return { c: " ", f: this.fg, b: this.bg, a: this.attr };
  }

  put(ch) {
    if (this.wrapNext && this.autowrap) {
      this.cx = 0;
      this.lineFeed();
      this.wrapNext = false;
    }
    const line = this.screen.lines[this.cy];
    if (!line) return;
    line[this.cx] = { c: ch, f: this.fg, b: this.bg, a: this.attr };
    this.dirty.add(this.cy);
    if (this.cx + 1 >= this.cols) this.wrapNext = true;
    else this.cx += 1;
  }

  lineFeed() {
    if (this.cy === this.bot) this.scrollUp(1);
    else this.cy = Math.min(this.rows - 1, this.cy + 1);
    this.wrapNext = false;
  }

  reverseIndex() {
    if (this.cy === this.top) this.scrollDown(1);
    else this.cy = Math.max(0, this.cy - 1);
  }

  scrollUp(n) {
    for (let i = 0; i < n; i++) {
      this.screen.lines.splice(this.top, 1);
      this.screen.lines.splice(this.bot, 0, this.screen.newLine());
    }
    this.allDirty();
  }

  scrollDown(n) {
    for (let i = 0; i < n; i++) {
      this.screen.lines.splice(this.bot, 1);
      this.screen.lines.splice(this.top, 0, this.screen.newLine());
    }
    this.allDirty();
  }

  insertLines(n) {
    if (this.cy < this.top || this.cy > this.bot) return;
    for (let i = 0; i < n; i++) {
      this.screen.lines.splice(this.bot, 1);
      this.screen.lines.splice(this.cy, 0, this.screen.newLine());
    }
    this.allDirty();
  }

  deleteLines(n) {
    if (this.cy < this.top || this.cy > this.bot) return;
    for (let i = 0; i < n; i++) {
      this.screen.lines.splice(this.cy, 1);
      this.screen.lines.splice(this.bot, 0, this.screen.newLine());
    }
    this.allDirty();
  }

  insertChars(n) {
    const line = this.screen.lines[this.cy];
    for (let i = 0; i < n; i++) {
      line.splice(this.cx, 0, this.cell());
      line.length = this.cols;
    }
    this.dirty.add(this.cy);
  }

  deleteChars(n) {
    const line = this.screen.lines[this.cy];
    for (let i = 0; i < n; i++) {
      line.splice(this.cx, 1);
      line.push(this.cell());
    }
    this.dirty.add(this.cy);
  }

  eraseChars(n) {
    const line = this.screen.lines[this.cy];
    for (let x = this.cx; x < Math.min(this.cols, this.cx + n); x++) line[x] = this.cell();
    this.dirty.add(this.cy);
  }

  eraseLine(mode) {
    const line = this.screen.lines[this.cy];
    const from = mode === 0 ? this.cx : 0;
    const to = mode === 1 ? this.cx + 1 : this.cols;
    for (let x = from; x < to; x++) line[x] = this.cell();
    this.dirty.add(this.cy);
  }

  eraseDisplay(mode) {
    const wipe = (y, from, to) => {
      const line = this.screen.lines[y];
      for (let x = from; x < to; x++) line[x] = this.cell();
      this.dirty.add(y);
    };
    if (mode === 0) {
      wipe(this.cy, this.cx, this.cols);
      for (let y = this.cy + 1; y < this.rows; y++) wipe(y, 0, this.cols);
    } else if (mode === 1) {
      for (let y = 0; y < this.cy; y++) wipe(y, 0, this.cols);
      wipe(this.cy, 0, this.cx + 1);
    } else {
      for (let y = 0; y < this.rows; y++) wipe(y, 0, this.cols);
    }
  }

  saveCursor() {
    this.saved = { cx: this.cx, cy: this.cy, attr: this.attr, fg: this.fg, bg: this.bg };
  }

  restoreCursor() {
    if (!this.saved) return;
    this.cx = Math.min(this.cols - 1, this.saved.cx);
    this.cy = Math.min(this.rows - 1, this.saved.cy);
    this.attr = this.saved.attr;
    this.fg = this.saved.fg;
    this.bg = this.saved.bg;
    this.wrapNext = false;
  }

  reset() {
    this.screen = new Screen(this.cols, this.rows);
    this.alt = null;
    this.cx = 0; this.cy = 0;
    this.attr = 0; this.fg = null; this.bg = null;
    this.top = 0; this.bot = this.rows - 1;
    this.autowrap = true; this.cursorOn = true; this.wrapNext = false;
    this.allDirty();
  }

  // ------------------------------------------------------------ drawing ----

  render() {
    if (!this.dirty.size) return;
    // Rows are their own elements so a redraw touches only what changed: a
    // TUI rewrites two lines a frame, and rebuilding fifty is what makes a
    // web terminal feel like a web page.
    while (this.out.children.length < this.rows) {
      const d = document.createElement("div");
      d.className = "ntrow";
      this.out.appendChild(d);
    }
    while (this.out.children.length > this.rows) this.out.lastChild.remove();
    for (const y of this.dirty) {
      if (y < 0 || y >= this.rows) continue;
      const el = this.out.children[y];
      if (el) el.innerHTML = this.rowHtml(y);
    }
    this.dirty.clear();
  }

  rowHtml(y) {
    const line = this.screen.lines[y];
    const showCursor = this.cursorOn && y === this.cy;
    let html = "";
    let run = "";
    let key = null;
    const style = (cell, cursor) => {
      let f = cell.f, b = cell.b;
      if (cell.a & ATTR.INVERSE) { const t = f; f = b || "var(--ntbg)"; b = t || "var(--ntfg)"; }
      if (cursor) { const t = f; f = b || "var(--ntbg)"; b = t || "var(--ntfg)"; }
      const bits = [];
      if (f) bits.push(`color:${f}`);
      if (b) bits.push(`background:${b}`);
      if (cell.a & ATTR.BOLD) bits.push("font-weight:700");
      if (cell.a & ATTR.DIM) bits.push("opacity:.6");
      if (cell.a & ATTR.ITALIC) bits.push("font-style:italic");
      if (cell.a & ATTR.UNDER) bits.push("text-decoration:underline");
      if (cell.a & ATTR.STRIKE) bits.push("text-decoration:line-through");
      if (cell.a & ATTR.HIDDEN) bits.push("visibility:hidden");
      return bits.join(";");
    };
    const flush = () => {
      if (!run) return;
      html += key ? `<span style="${key}">${esc(run)}</span>` : esc(run);
      run = "";
    };
    for (let x = 0; x < this.cols; x++) {
      const cell = line[x] || blank();
      const k = style(cell, showCursor && x === this.cx);
      if (k !== key) { flush(); key = k; }
      run += cell.c;
    }
    flush();
    return html || "&nbsp;";
  }

  // ----------------------------------------------------------- keyboard ----

  onPaste(e) {
    const text = (e.clipboardData || window.clipboardData).getData("text");
    e.preventDefault();
    if (!text) return;
    // Bracketed paste, when the program asked for it: without the markers a
    // multi-line paste is read as a series of submits, which in an agent CLI
    // sends half-written instructions.
    this.send(this.bracketed ? `\x1b[200~${text}\x1b[201~` : text);
  }

  onKey(e) {
    if (e.key === "Unidentified") return;
    const k = e.key;
    const ctrl = e.ctrlKey, alt = e.altKey, shift = e.shiftKey, meta = e.metaKey;

    // Leave the browser's own shortcuts alone: copy, paste, new tab, reload.
    if (meta) return;
    if (ctrl && (k === "c" || k === "v") && shift) return;

    const put = (s) => { e.preventDefault(); this.send(s); };

    const keys = {
      Enter: "\r", Tab: "\t", Backspace: "\x7f", Escape: "\x1b",
      ArrowUp: "\x1b[A", ArrowDown: "\x1b[B", ArrowRight: "\x1b[C", ArrowLeft: "\x1b[D",
      Home: "\x1b[H", End: "\x1b[F", Insert: "\x1b[2~", Delete: "\x1b[3~",
      PageUp: "\x1b[5~", PageDown: "\x1b[6~",
      F1: "\x1bOP", F2: "\x1bOQ", F3: "\x1bOR", F4: "\x1bOS",
      F5: "\x1b[15~", F6: "\x1b[17~", F7: "\x1b[18~", F8: "\x1b[19~",
      F9: "\x1b[20~", F10: "\x1b[21~", F11: "\x1b[23~", F12: "\x1b[24~",
    };
    if (ctrl && k.length === 1) {
      const c = k.toLowerCase();
      if (c >= "a" && c <= "z") return put(String.fromCharCode(c.charCodeAt(0) - 96));
      if (c === " ") return put("\x00");
      if (c === "[") return put("\x1b");
      if (c === "\\") return put("\x1c");
      if (c === "]") return put("\x1d");
    }
    if (keys[k]) {
      // Shift+Tab is its own sequence, and TUIs use it to go backwards.
      if (k === "Tab" && shift) return put("\x1b[Z");
      return put(keys[k]);
    }
    if (alt && k.length === 1) return put("\x1b" + k);
    if (k.length === 1) return put(k);
  }
}

function esc(s) {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

window.NexTerm = NexTerm;
})();
