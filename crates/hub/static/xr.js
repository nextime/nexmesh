// nexmesh -- the fleet as a room you can stand in.
//
// Copyright (C) 2026 Stefy Lanza <stefy@nexlab.net>
//
// This program is free software: you can redistribute it and/or modify it
// under the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version. This program is distributed WITHOUT ANY WARRANTY; see
// the LICENSE file, or <https://www.gnu.org/licenses/>, for the full terms.
//
// WebGL2 and WebXR, by hand. No library, for the same reason the dashboard
// has no CDN: a fleet hub may sit on a network with no route to the
// internet, and a headset that cannot load a 600KB bundle is a headset that
// shows nothing. Everything here is procedural geometry, a canvas for text,
// and about four hundred lines of matrix arithmetic.
//
// Two modes, because a headset is two different tools:
//   2D    -- the ordinary dashboard in a floating window, over BigscreenVR
//            or whatever else is running. That is `index.html`; this file is
//            reached from it.
//   XR    -- immersive: the team standing around you, the code graph as a
//            thing with a far side, pointing to select and talking to it.
// Without a headset the same scene renders to the canvas with a mouse, which
// is what the screenshots in the README are made from.

"use strict";

// ----------------------------------------------------------------- maths --
// Column-major 4x4, the layout WebGL wants.

const M4 = {
  ident: () => new Float32Array([1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1]),
  mul(a, b) {
    const o = new Float32Array(16);
    for (let c = 0; c < 4; c++) {
      for (let r = 0; r < 4; r++) {
        let s = 0;
        for (let k = 0; k < 4; k++) s += a[k * 4 + r] * b[c * 4 + k];
        o[c * 4 + r] = s;
      }
    }
    return o;
  },
  translate(x, y, z) {
    const m = M4.ident();
    m[12] = x; m[13] = y; m[14] = z;
    return m;
  },
  scale(x, y, z) {
    const m = M4.ident();
    m[0] = x; m[5] = y; m[10] = z;
    return m;
  },
  rotY(a) {
    const m = M4.ident(), c = Math.cos(a), s = Math.sin(a);
    m[0] = c; m[2] = -s; m[8] = s; m[10] = c;
    return m;
  },
  rotX(a) {
    const m = M4.ident(), c = Math.cos(a), s = Math.sin(a);
    m[5] = c; m[6] = s; m[9] = -s; m[10] = c;
    return m;
  },
  perspective(fovy, aspect, near, far) {
    const f = 1 / Math.tan(fovy / 2), m = new Float32Array(16);
    m[0] = f / aspect; m[5] = f; m[11] = -1;
    m[10] = (far + near) / (near - far);
    m[14] = (2 * far * near) / (near - far);
    return m;
  },
  lookAt(eye, at, up) {
    const z = V3.norm(V3.sub(eye, at));
    const x = V3.norm(V3.cross(up, z));
    const y = V3.cross(z, x);
    return new Float32Array([
      x[0], y[0], z[0], 0,
      x[1], y[1], z[1], 0,
      x[2], y[2], z[2], 0,
      -V3.dot(x, eye), -V3.dot(y, eye), -V3.dot(z, eye), 1,
    ]);
  },
  // Enough of an inverse for a rigid transform plus uniform scale, which is
  // all this scene ever builds.
  invert(m) {
    const o = new Float32Array(16);
    const a00=m[0],a01=m[1],a02=m[2],a03=m[3];
    const a10=m[4],a11=m[5],a12=m[6],a13=m[7];
    const a20=m[8],a21=m[9],a22=m[10],a23=m[11];
    const a30=m[12],a31=m[13],a32=m[14],a33=m[15];
    const b00=a00*a11-a01*a10, b01=a00*a12-a02*a10, b02=a00*a13-a03*a10;
    const b03=a01*a12-a02*a11, b04=a01*a13-a03*a11, b05=a02*a13-a03*a12;
    const b06=a20*a31-a21*a30, b07=a20*a32-a22*a30, b08=a20*a33-a23*a30;
    const b09=a21*a32-a22*a31, b10=a21*a33-a23*a31, b11=a22*a33-a23*a32;
    let det = b00*b11-b01*b10+b02*b09+b03*b08-b04*b07+b05*b06;
    if (!det) return M4.ident();
    det = 1 / det;
    o[0]=(a11*b11-a12*b10+a13*b09)*det;  o[1]=(a02*b10-a01*b11-a03*b09)*det;
    o[2]=(a31*b05-a32*b04+a33*b03)*det;  o[3]=(a22*b04-a21*b05-a23*b03)*det;
    o[4]=(a12*b08-a10*b11-a13*b07)*det;  o[5]=(a00*b11-a02*b08+a03*b07)*det;
    o[6]=(a32*b02-a30*b05-a33*b01)*det;  o[7]=(a20*b05-a22*b02+a23*b01)*det;
    o[8]=(a10*b10-a11*b08+a13*b06)*det;  o[9]=(a01*b08-a00*b10-a03*b06)*det;
    o[10]=(a30*b04-a31*b02+a33*b00)*det; o[11]=(a21*b02-a20*b04-a23*b00)*det;
    o[12]=(a11*b07-a10*b09-a12*b06)*det; o[13]=(a00*b09-a01*b07+a02*b06)*det;
    o[14]=(a31*b01-a30*b03-a32*b00)*det; o[15]=(a20*b03-a21*b01+a22*b00)*det;
    return o;
  },
  apply(m, v) {
    return [
      m[0]*v[0] + m[4]*v[1] + m[8]*v[2] + m[12],
      m[1]*v[0] + m[5]*v[1] + m[9]*v[2] + m[13],
      m[2]*v[0] + m[6]*v[1] + m[10]*v[2] + m[14],
    ];
  },
  dir(m, v) {
    return [
      m[0]*v[0] + m[4]*v[1] + m[8]*v[2],
      m[1]*v[0] + m[5]*v[1] + m[9]*v[2],
      m[2]*v[0] + m[6]*v[1] + m[10]*v[2],
    ];
  },
};

const V3 = {
  sub: (a, b) => [a[0]-b[0], a[1]-b[1], a[2]-b[2]],
  add: (a, b) => [a[0]+b[0], a[1]+b[1], a[2]+b[2]],
  mul: (a, s) => [a[0]*s, a[1]*s, a[2]*s],
  dot: (a, b) => a[0]*b[0] + a[1]*b[1] + a[2]*b[2],
  cross: (a, b) => [a[1]*b[2]-a[2]*b[1], a[2]*b[0]-a[0]*b[2], a[0]*b[1]-a[1]*b[0]],
  len: (a) => Math.hypot(a[0], a[1], a[2]),
  norm(a) {
    const l = V3.len(a) || 1;
    return [a[0]/l, a[1]/l, a[2]/l];
  },
};

// -------------------------------------------------------------- geometry --
// Every shape is built once, as interleaved position+normal, and drawn with
// a model matrix. A teammate's SHAPE says which CLI it is, so a glance
// across the room is enough: the colour is already carrying its state.

function meshData(name) {
  const tri = (a, b, c) => {
    const n = V3.norm(V3.cross(V3.sub(b, a), V3.sub(c, a)));
    return [...a, ...n, ...b, ...n, ...c, ...n];
  };
  const quad = (a, b, c, d) => [...tri(a, b, c), ...tri(a, c, d)];
  const out = [];
  if (name === "box") {
    const p = [[-.5,-.5,-.5],[.5,-.5,-.5],[.5,.5,-.5],[-.5,.5,-.5],
               [-.5,-.5,.5],[.5,-.5,.5],[.5,.5,.5],[-.5,.5,.5]];
    out.push(...quad(p[4],p[5],p[6],p[7]), ...quad(p[1],p[0],p[3],p[2]),
             ...quad(p[0],p[4],p[7],p[3]), ...quad(p[5],p[1],p[2],p[6]),
             ...quad(p[3],p[7],p[6],p[2]), ...quad(p[0],p[1],p[5],p[4]));
  } else if (name === "octa" || name === "icosa" || name === "sphere") {
    // One octahedron, subdivided: 0 gives the octahedron itself, 2 is round
    // enough to read as a sphere at two metres.
    const depth = name === "octa" ? 0 : (name === "icosa" ? 1 : 2);
    let faces = [
      [[1,0,0],[0,1,0],[0,0,1]], [[0,1,0],[-1,0,0],[0,0,1]],
      [[-1,0,0],[0,-1,0],[0,0,1]], [[0,-1,0],[1,0,0],[0,0,1]],
      [[0,1,0],[1,0,0],[0,0,-1]], [[-1,0,0],[0,1,0],[0,0,-1]],
      [[0,-1,0],[-1,0,0],[0,0,-1]], [[1,0,0],[0,-1,0],[0,0,-1]],
    ];
    for (let d = 0; d < depth; d++) {
      const next = [];
      for (const [a, b, c] of faces) {
        const ab = V3.norm(V3.add(a, b)), bc = V3.norm(V3.add(b, c)),
              ca = V3.norm(V3.add(c, a));
        next.push([a, ab, ca], [ab, b, bc], [ca, bc, c], [ab, bc, ca]);
      }
      faces = next;
    }
    for (const [a, b, c] of faces) {
      // A sphere's normal IS its position, which keeps the shading smooth
      // instead of faceted.
      const h = V3.mul(a, .5), i = V3.mul(b, .5), j = V3.mul(c, .5);
      out.push(...h, ...a, ...i, ...b, ...j, ...c);
    }
  } else if (name === "tetra") {
    const p = [[0,.6,0],[-.5,-.3,.4],[.5,-.3,.4],[0,-.3,-.6]];
    out.push(...tri(p[0],p[1],p[2]), ...tri(p[0],p[2],p[3]),
             ...tri(p[0],p[3],p[1]), ...tri(p[1],p[3],p[2]));
  } else if (name === "cyl" || name === "ring") {
    const seg = 24, r = name === "ring" ? .5 : .35, h = name === "ring" ? .14 : .5;
    const inner = name === "ring" ? .3 : 0;
    for (let i = 0; i < seg; i++) {
      const a = (i / seg) * Math.PI * 2, b = ((i + 1) / seg) * Math.PI * 2;
      const c1 = [Math.cos(a)*r, 0, Math.sin(a)*r], c2 = [Math.cos(b)*r, 0, Math.sin(b)*r];
      const t1 = [c1[0], h, c1[2]], t2 = [c2[0], h, c2[2]];
      out.push(...quad([c1[0],-h,c1[2]], [c2[0],-h,c2[2]], t2, t1));
      if (inner) {
        const i1 = [Math.cos(a)*inner, 0, Math.sin(a)*inner];
        const i2 = [Math.cos(b)*inner, 0, Math.sin(b)*inner];
        out.push(...quad([i2[0],-h,i2[2]], [i1[0],-h,i1[2]],
                         [i1[0],h,i1[2]], [i2[0],h,i2[2]]));
        out.push(...quad([i1[0],h,i1[2]], t1, t2, [i2[0],h,i2[2]]));
        out.push(...quad([i2[0],-h,i2[2]], [c2[0],-h,c2[2]],
                         [c1[0],-h,c1[2]], [i1[0],-h,i1[2]]));
      } else {
        out.push(...tri([0,h,0], t1, t2), ...tri([0,-h,0], [c2[0],-h,c2[2]], [c1[0],-h,c1[2]]));
      }
    }
  } else if (name === "quad") {
    out.push(...quad([-.5,-.5,0],[.5,-.5,0],[.5,.5,0],[-.5,.5,0]));
  }
  return new Float32Array(out);
}

// Which shape stands for which CLI. A room of identical boxes tells you
// nothing; this is the difference between "four agents" and "claude, codex,
// opencode and the bridge".
function shapeFor(a) {
  const cli = (a.cli || "").toLowerCase();
  if (a.kind === "external" || a.provenance === "external") return "ring";
  if (cli.startsWith("claude")) return "octa";
  if (cli.startsWith("codex")) return "box";
  if (cli.startsWith("opencode")) return "sphere";
  if (cli.startsWith("qwen")) return "tetra";
  if (cli.startsWith("gemini")) return "icosa";
  if (cli.startsWith("hermes")) return "cyl";
  if (cli === "tmux") return "box";
  return "icosa";
}

// Colour says STATE, and only state. Reading a room should not need a key.
function colourFor(a) {
  if (a.paused) return [.45, .45, .5];
  switch (a.task_state) {
    case "working": return [.30, .78, .52];
    case "done_pending_review": return [.95, .70, .25];
    case "addressing_feedback": return [.95, .55, .25];
    case "approved": return [.40, .66, .95];
    case "released": return [.40, .40, .46];
  }
  return (a.status === "running" || a.status === "connected")
    ? [.40, .66, .95] : [.42, .42, .48];
}

// --------------------------------------------------------------- renderer --

const VERT = `#version 300 es
layout(location=0) in vec3 pos;
layout(location=1) in vec3 nrm;
uniform mat4 uProj, uView, uModel;
out vec3 vNormal;
out vec3 vWorld;
void main() {
  vec4 w = uModel * vec4(pos, 1.0);
  vWorld = w.xyz;
  vNormal = mat3(uModel) * nrm;
  gl_Position = uProj * uView * w;
}`;

const FRAG = `#version 300 es
precision highp float;
in vec3 vNormal;
in vec3 vWorld;
uniform vec3 uColour;
uniform float uGlow;
out vec4 frag;
void main() {
  vec3 n = normalize(vNormal);
  // Two lights and a floor bounce: enough to read a shape's form without
  // any of the machinery a real engine would bring.
  float key = max(dot(n, normalize(vec3(0.4, 0.9, 0.3))), 0.0);
  float fill = max(dot(n, normalize(vec3(-0.6, 0.2, -0.5))), 0.0) * 0.35;
  float bounce = max(-n.y, 0.0) * 0.12;
  vec3 c = uColour * (0.28 + key * 0.75 + fill + bounce);
  // Rim light, so an object keeps its outline against a dark room.
  float rim = pow(1.0 - max(dot(n, normalize(-vWorld)), 0.0), 3.0);
  c += uColour * rim * 0.25;
  c = mix(c, vec3(1.0), uGlow * 0.55);
  frag = vec4(c, 1.0);
}`;

const TEX_VERT = `#version 300 es
layout(location=0) in vec3 pos;
layout(location=1) in vec3 nrm;
uniform mat4 uProj, uView, uModel;
out vec2 vUv;
void main() {
  vUv = vec2(pos.x + 0.5, 0.5 - pos.y);
  gl_Position = uProj * uView * uModel * vec4(pos, 1.0);
}`;

const TEX_FRAG = `#version 300 es
precision highp float;
in vec2 vUv;
uniform sampler2D uTex;
uniform float uAlpha;
out vec4 frag;
void main() {
  vec4 t = texture(uTex, vUv);
  if (t.a < 0.02) discard;
  frag = vec4(t.rgb, t.a * uAlpha);
}`;

function compile(gl, vsrc, fsrc) {
  const mk = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) {
      throw new Error("shader: " + gl.getShaderInfoLog(s));
    }
    return s;
  };
  const p = gl.createProgram();
  gl.attachShader(p, mk(gl.VERTEX_SHADER, vsrc));
  gl.attachShader(p, mk(gl.FRAGMENT_SHADER, fsrc));
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
    throw new Error("link: " + gl.getProgramInfoLog(p));
  }
  return p;
}

class Renderer {
  constructor(canvas) {
    this.canvas = canvas;
    this.gl = canvas.getContext("webgl2", {
      antialias: true, alpha: false, xrCompatible: true, depth: true,
    });
    if (!this.gl) throw new Error("this browser has no WebGL2");
    const gl = this.gl;
    this.prog = compile(gl, VERT, FRAG);
    this.texProg = compile(gl, TEX_VERT, TEX_FRAG);
    this.u = {
      proj: gl.getUniformLocation(this.prog, "uProj"),
      view: gl.getUniformLocation(this.prog, "uView"),
      model: gl.getUniformLocation(this.prog, "uModel"),
      colour: gl.getUniformLocation(this.prog, "uColour"),
      glow: gl.getUniformLocation(this.prog, "uGlow"),
    };
    this.tu = {
      proj: gl.getUniformLocation(this.texProg, "uProj"),
      view: gl.getUniformLocation(this.texProg, "uView"),
      model: gl.getUniformLocation(this.texProg, "uModel"),
      tex: gl.getUniformLocation(this.texProg, "uTex"),
      alpha: gl.getUniformLocation(this.texProg, "uAlpha"),
    };
    this.meshes = {};
    for (const n of ["box", "octa", "icosa", "sphere", "tetra", "cyl", "ring", "quad"]) {
      this.meshes[n] = this.upload(meshData(n));
    }
    gl.enable(gl.DEPTH_TEST);
    gl.enable(gl.CULL_FACE);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }

  upload(data) {
    const gl = this.gl;
    const vao = gl.createVertexArray();
    gl.bindVertexArray(vao);
    const buf = gl.createBuffer();
    gl.bindBuffer(gl.ARRAY_BUFFER, buf);
    gl.bufferData(gl.ARRAY_BUFFER, data, gl.STATIC_DRAW);
    gl.enableVertexAttribArray(0);
    gl.vertexAttribPointer(0, 3, gl.FLOAT, false, 24, 0);
    gl.enableVertexAttribArray(1);
    gl.vertexAttribPointer(1, 3, gl.FLOAT, false, 24, 12);
    gl.bindVertexArray(null);
    return { vao, count: data.length / 6 };
  }

  // Text as a texture. A headset renders a 2D canvas perfectly well; what it
  // will not do is give you a font engine in GLSL.
  label(text, opts) {
    const o = opts || {};
    const gl = this.gl;
    const pad = 16, size = o.size || 44;
    const c = document.createElement("canvas");
    const ctx = c.getContext("2d");
    ctx.font = `${o.weight || 600} ${size}px ui-monospace, "DejaVu Sans Mono", monospace`;
    const lines = String(text).split("\n");
    const w = Math.max(...lines.map((l) => ctx.measureText(l).width)) + pad * 2;
    c.width = Math.max(16, Math.ceil(w));
    c.height = Math.ceil(lines.length * size * 1.3 + pad * 2);
    const x = c.getContext("2d");
    x.font = `${o.weight || 600} ${size}px ui-monospace, "DejaVu Sans Mono", monospace`;
    x.textBaseline = "top";
    if (o.bg !== false) {
      x.fillStyle = o.bg || "rgba(10,12,18,0.82)";
      const r = 14;
      x.beginPath();
      x.moveTo(r, 0); x.arcTo(c.width, 0, c.width, c.height, r);
      x.arcTo(c.width, c.height, 0, c.height, r); x.arcTo(0, c.height, 0, 0, r);
      x.arcTo(0, 0, c.width, 0, r); x.closePath(); x.fill();
    }
    x.fillStyle = o.fg || "#e8ecf4";
    lines.forEach((l, i) => x.fillText(l, pad, pad + i * size * 1.3));
    const tex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, c);
    gl.generateMipmap(gl.TEXTURE_2D);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return { tex, w: c.width, h: c.height, aspect: c.width / c.height };
  }

  begin(viewport, clear) {
    const gl = this.gl;
    gl.viewport(viewport.x, viewport.y, viewport.width, viewport.height);
    if (clear) {
      gl.clearColor(0.035, 0.04, 0.06, 1);
      gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    }
  }

  draw(mesh, model, colour, glow, proj, view) {
    const gl = this.gl;
    gl.useProgram(this.prog);
    gl.uniformMatrix4fv(this.u.proj, false, proj);
    gl.uniformMatrix4fv(this.u.view, false, view);
    gl.uniformMatrix4fv(this.u.model, false, model);
    gl.uniform3fv(this.u.colour, colour);
    gl.uniform1f(this.u.glow, glow || 0);
    const m = this.meshes[mesh] || this.meshes.box;
    gl.bindVertexArray(m.vao);
    gl.drawArrays(gl.TRIANGLES, 0, m.count);
    gl.bindVertexArray(null);
  }

  drawLabel(label, model, proj, view, alpha) {
    const gl = this.gl;
    gl.useProgram(this.texProg);
    gl.uniformMatrix4fv(this.tu.proj, false, proj);
    gl.uniformMatrix4fv(this.tu.view, false, view);
    gl.uniformMatrix4fv(this.tu.model, false, model);
    gl.uniform1f(this.tu.alpha, alpha === undefined ? 1 : alpha);
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, label.tex);
    gl.uniform1i(this.tu.tex, 0);
    gl.disable(gl.CULL_FACE);
    const m = this.meshes.quad;
    gl.bindVertexArray(m.vao);
    gl.drawArrays(gl.TRIANGLES, 0, m.count);
    gl.bindVertexArray(null);
    gl.enable(gl.CULL_FACE);
  }
}

// ------------------------------------------------------------------ scene --
// What is in the room, and where. Rebuilt whenever the snapshot changes,
// which is every couple of seconds, so it has to be cheap and stable: an
// agent must not jump across the room because another one appeared.

class Scene {
  constructor(renderer) {
    this.r = renderer;
    this.nodes = [];          // selectable things
    this.decor = [];          // floor, rings, anything you cannot point at
    this.labels = new Map();  // text -> texture, so a label is made once
    this.selected = null;
    this.hovered = null;
    this.mode = "team";       // team | graph
  }

  labelFor(text, opts) {
    const key = (opts && opts.key) || text;
    let l = this.labels.get(key);
    if (!l) {
      l = this.r.label(text, opts);
      this.labels.set(key, l);
    }
    return l;
  }

  // The team, standing in an arc in front of you. Sessions are separate
  // arcs at different radii, which is what makes "who is on which team"
  // something you can see from where you are standing rather than read.
  build(snap) {
    const agents = ((snap && snap.detail && snap.detail.agents) || [])
      .filter((a) => a.cli !== "tmux");
    const sessions = [];
    for (const a of agents) {
      const s = a.session || "";
      if (!sessions.includes(s)) sessions.push(s);
    }
    sessions.sort();
    this.nodes = [];
    this.decor = [];

    // A floor, so "down" means something and the room has a size, and a
    // grid on it, because a plain dark plane gives the eye nothing to judge
    // distance by -- which in a headset is the difference between a room and
    // a void with objects in it.
    this.decor.push({
      mesh: "box", colour: [.055, .065, .09], glow: 0,
      model: M4.mul(M4.translate(0, -1.42, -1), M4.scale(16, .04, 16)),
    });
    for (let i = -6; i <= 6; i++) {
      const faint = [.10, .12, .17];
      this.decor.push({
        mesh: "box", colour: faint, glow: 0,
        model: M4.mul(M4.translate(i * 0.9, -1.39, -1), M4.scale(.012, .005, 11)),
      });
      this.decor.push({
        mesh: "box", colour: faint, glow: 0,
        model: M4.mul(M4.translate(0, -1.39, -1 + i * 0.9), M4.scale(11, .005, .012)),
      });
    }

    // One SECTOR per session, side by side, rather than concentric rings:
    // concentric put one team's heads between another's and read as a
    // single crowd. Standing apart is what two teams look like.
    const sectorWidth = Math.min(1.5, (Math.PI * 1.25) / Math.max(sessions.length, 1));
    const firstMiddle = -((sessions.length - 1) / 2) * sectorWidth;
    sessions.forEach((sname, si) => {
      const mine = agents.filter((a) => (a.session || "") === sname);
      const radius = 2.0 + (si % 2) * 0.25;
      const middle = firstMiddle + si * sectorWidth;
      const spread = Math.min(sectorWidth * 0.82, 0.34 * Math.max(mine.length - 1, 1));
      mine.forEach((a, i) => {
        const t = mine.length === 1 ? 0 : (i / (mine.length - 1) - 0.5);
        const ang = middle + t * spread;
        const x = Math.sin(ang) * radius;
        const z = -Math.cos(ang) * radius;
        const busy = a.task_state === "working";
        this.nodes.push({
          kind: "agent",
          data: a,
          // Every other one a little higher, so two names side by side are
          // never on the same line.
          pos: [x, (i % 2 ? 0.16 : -0.04) - si * 0.05, z],
          size: 0.34,
          mesh: shapeFor(a),
          colour: colourFor(a),
          spin: busy ? 0.6 : 0.08,       // a working agent turns; an idle one drifts
          bob: busy ? 0.03 : 0.012,
          label: a.name,
          sub: `${a.role || a.cli || ""}${a.host && a.host !== "-" ? "  @" + a.host : ""}`,
        });
      });
      if (mine.length) {
        this.decor.push({
          mesh: "ring", colour: [.17, .20, .28], glow: 0,
          model: M4.mul(M4.translate(0, -1.35, 0), M4.scale(radius * 2, .5, radius * 2)),
        });
        this.nodes.push({
          kind: "session", data: { session: sname },
          pos: [Math.sin(middle) * (radius + .95), -1.16, -Math.cos(middle) * (radius + .95)],
          size: 0.001, mesh: "quad", colour: [.3, .3, .3], spin: 0, bob: 0,
          label: "#" + (sname || "session"), sub: `${mine.length} seat(s)`,
          flat: true,
        });
      }
    });
    this.sessions = sessions;
    this.buildGraph(snap);
  }

  // The code graph, as a thing with a far side. Subsystems on a sphere
  // rather than a ring: a ring of twenty is a line of twenty, and the
  // point of standing inside a map is being able to look around it.
  buildGraph(snap) {
    const g = ((snap && snap.detail && snap.detail.graph) || [])[0];
    const subs = (g && g.subsystems) || [];
    this.graph = [];
    this.graphLinks = [];
    if (!subs.length) return;
    const n = subs.length;
    // Fibonacci sphere: evenly spread without clumping at the poles.
    const golden = Math.PI * (3 - Math.sqrt(5));
    subs.forEach((s, i) => {
      const y = 1 - (i / Math.max(n - 1, 1)) * 2;
      const r = Math.sqrt(Math.max(0, 1 - y * y));
      const th = golden * i;
      const size = Math.min(.5, .14 + Math.log2((s.members || 1) + 1) * .06);
      this.graph.push({
        kind: "subsystem",
        data: s,
        pos: [Math.cos(th) * r * 1.7, y * 1.05 + 0.25, Math.sin(th) * r * 1.7 - 2.1],
        size,
        mesh: "sphere",
        colour: [.42, .55, .85],
        spin: 0.05,
        bob: 0,
        label: s.name || `subsystem ${i}`,
        sub: `${s.members || 0} symbol(s)`,
      });
    });
    // What depends on what. A cloud of spheres is not a graph; the lines
    // are the thing you came to look at, and their weight is how much
    // crosses between two subsystems. A dotted-looking thin line is a
    // GUESS -- a call resolved by name -- and a solid one is a fact read
    // off the tree, the same distinction the flat graph makes.
    const byName = new Map(this.graph.map((o) => [o.data.name, o]));
    const links = (g && g.links) || [];
    const heaviest = Math.max(1, ...links.map((l) => l.n || 1));
    for (const l of links) {
      const a = byName.get(l.a), b = byName.get(l.b);
      if (!a || !b || a === b) continue;
      this.graphLinks.push({
        a, b,
        weight: (l.n || 1) / heaviest,
        certain: !!l.certain,
      });
    }
  }

  active() {
    return this.mode === "graph" ? (this.graph || []) : this.nodes;
  }

  // What is the pointer on? A ray against each object's bounding sphere:
  // exact enough for objects this size, and it costs nothing to do per
  // frame for both hands.
  hit(origin, dir) {
    let best = null, bestT = Infinity;
    for (const o of this.active()) {
      if (o.flat) continue;          // a sign on the floor is not a teammate
      const m = V3.sub(o.pos, origin);
      const tca = V3.dot(m, dir);
      if (tca < 0) continue;
      const d2 = V3.dot(m, m) - tca * tca;
      const r = o.size * 1.25;
      if (d2 > r * r) continue;
      const thc = Math.sqrt(r * r - d2);
      const t = tca - thc;
      if (t < bestT) { bestT = t; best = o; }
    }
    return best ? { object: best, t: bestT } : null;
  }

  render(proj, view, eye, now) {
    const r = this.r;
    for (const d of this.decor) r.draw(d.mesh, d.model, d.colour, 0, proj, view);
    if (this.mode === "graph") this.renderLinks(proj, view);
    for (const o of this.active()) {
      if (o.flat) {
        const lab = this.labelFor(o.label, { key: "sess:" + o.label, size: 38,
                                             fg: "#7f93ad", bg: "rgba(10,12,18,0.55)" });
        const h = 0.14, w = h * lab.aspect;
        r.drawLabel(lab, M4.mul(billboard(o.pos, eye), M4.scale(w, h, 1)), proj, view, .85);
        continue;
      }
      const bob = Math.sin(now * 0.001 + o.pos[0] * 2) * (o.bob || 0);
      const spin = M4.rotY(now * 0.001 * (o.spin || 0));
      const here = M4.translate(o.pos[0], o.pos[1] + bob, o.pos[2]);
      const s = o.size * (o === this.selected ? 1.18 : 1);
      const model = M4.mul(M4.mul(here, spin), M4.scale(s, s, s));
      const glow = o === this.selected ? .5 : (o === this.hovered ? .25 : 0);
      if (o === this.selected || o === this.hovered) {
        // A lit ring under the chosen one. Glow alone is ambiguous at a
        // distance, and in a headset "which one did I just point at" has to
        // be answerable without reading anything.
        const pulse = 1 + Math.sin(now * 0.004) * 0.05;
        const ring = M4.mul(
          M4.translate(o.pos[0], o.pos[1] + bob - o.size - 0.06, o.pos[2]),
          M4.scale(o.size * 2.5 * pulse, 0.12, o.size * 2.5 * pulse));
        r.draw("ring", ring, o === this.selected ? [.55, .85, 1] : [.4, .5, .62],
               o === this.selected ? .45 : .15, proj, view);
      }
      r.draw(o.mesh, model, o.colour, glow, proj, view);

      // The name, always facing you. A label you have to walk round is not
      // a label.
      const lab = this.labelFor(o.label, { size: 46 });
      const h = (this.mode === "graph" ? 0.115 : 0.155), w = h * lab.aspect;
      const lp = [o.pos[0], o.pos[1] + bob + o.size + 0.24, o.pos[2]];
      r.drawLabel(lab, M4.mul(billboard(lp, eye), M4.scale(w, h, 1)), proj, view, 1);

      if (o.sub && (o === this.selected || o === this.hovered)) {
        const sub = this.labelFor(o.sub, { size: 34, key: "sub:" + o.sub, fg: "#9fb0c8" });
        const sh = 0.095, sw = sh * sub.aspect;
        const sp = [o.pos[0], o.pos[1] + bob - o.size - 0.16, o.pos[2]];
        r.drawLabel(sub, M4.mul(billboard(sp, eye), M4.scale(sw, sh, 1)), proj, view, .95);
      }
    }
  }
}

// The dependencies, drawn as thin boxes between two spheres. A line
// primitive would be one pixel wide at any distance, which in a headset
// disappears; a box has thickness and so has presence.
Scene.prototype.renderLinks = function (proj, view) {
  for (const l of this.graphLinks || []) {
    const lit = this.selected === l.a || this.selected === l.b ||
                this.hovered === l.a || this.hovered === l.b;
    const d = V3.sub(l.b.pos, l.a.pos);
    const len = V3.len(d);
    if (!len) continue;
    const dir = V3.mul(d, 1 / len);
    const mid = V3.add(l.a.pos, V3.mul(d, 0.5));
    const yaw = Math.atan2(dir[0], dir[2]);
    const pitch = Math.asin(Math.max(-1, Math.min(1, -dir[1])));
    const w = (l.certain ? 0.012 : 0.006) + l.weight * 0.02;
    const m = M4.mul(
      M4.mul(M4.mul(M4.translate(mid[0], mid[1], mid[2]), M4.rotY(yaw)), M4.rotX(-pitch)),
      M4.scale(w, w, len));
    // The cursor's own edges are picked out, exactly as the console does
    // it: everything else is there for shape, not for reading.
    const c = lit ? [.55, .85, 1] : (l.certain ? [.24, .30, .44] : [.20, .22, .30]);
    this.r.draw("box", m, c, lit ? .35 : 0, proj, view);
  }
};

// A quad that faces the eye, turning only about Y so text never tilts.
function billboard(pos, eye) {
  const d = V3.sub(eye, pos);
  const ang = Math.atan2(d[0], d[2]);
  return M4.mul(M4.translate(pos[0], pos[1], pos[2]), M4.rotY(ang));
}

// -------------------------------------------------------------------- app --

const KEY = "nexmesh-token";
let token = "";
try { token = localStorage.getItem(KEY) || ""; } catch (e) { token = ""; }
const params = new URLSearchParams(location.search);
const DEMO = params.get("demo") === "1";

async function api(path, body) {
  const headers = { "content-type": "application/json" };
  if (token) headers.authorization = "Bearer " + token;
  const r = await fetch(path, {
    method: body ? "POST" : "GET",
    headers,
    credentials: "same-origin",
    cache: "no-store",
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) throw new Error(`${path}: ${r.status}`);
  return r.json();
}

// A fleet to look at when there is no fleet to look at: the screenshots in
// the README, and anybody opening this on a hub they have not paired with.
// Deliberately not real hostnames -- machine-a, machine-b, like every other
// example in this repository.
function demoSnapshot() {
  const mk = (name, cli, role, state, host, session) =>
    ({ name, cli, role, task_state: state, status: "running", host, session,
       provenance: "managed", paused: false });
  return { detail: {
    agents: [
      mk("Ada", "claude", "architect", "working", "machine-a", "payments"),
      mk("Max", "claude", "engineer", "working", "machine-a", "payments"),
      mk("Iris", "opencode", "code reviewer", "done_pending_review", "machine-b", "payments"),
      mk("Rune", "codex", "engineer", "working", "machine-b", "payments"),
      mk("Quill", "qwen", "doc writer", "approved", "machine-b", "docs-site"),
      mk("Hermes", "hermes", "archivist", "idle", "machine-c", "docs-site"),
      { name: "CoderAI", cli: "bridge", role: "capability bridge", task_state: "idle",
        status: "running", host: "machine-c", session: "payments",
        provenance: "external", paused: false },
    ],
    graph: [{ subsystems: [
      { name: "auth", members: 34 }, { name: "wire", members: 51 },
      { name: "storage", members: 28 }, { name: "render", members: 19 },
      { name: "cli", members: 42 }, { name: "node", members: 37 },
      { name: "hub", members: 66 }, { name: "memory", members: 15 },
      { name: "graph", members: 23 }, { name: "voice", members: 12 },
    ], links: [
      { a: "hub", b: "wire", n: 41, certain: true },
      { a: "hub", b: "storage", n: 33, certain: true },
      { a: "hub", b: "auth", n: 22, certain: true },
      { a: "node", b: "wire", n: 38, certain: true },
      { a: "node", b: "cli", n: 17, certain: false },
      { a: "cli", b: "render", n: 24, certain: true },
      { a: "cli", b: "voice", n: 12, certain: true },
      { a: "hub", b: "memory", n: 15, certain: true },
      { a: "hub", b: "graph", n: 19, certain: true },
      { a: "graph", b: "storage", n: 9, certain: false },
      { a: "auth", b: "storage", n: 7, certain: true },
      { a: "render", b: "graph", n: 6, certain: false },
    ] }],
  } };
}

class App {
  constructor() {
    this.canvas = document.getElementById("gl");
    this.r = new Renderer(this.canvas);
    this.scene = new Scene(this.r);
    this.snap = null;
    this.xr = null;
    this.refSpace = null;
    // Where you are standing when there is no headset. Orbiting, because a
    // screenshot of a room wants to move through it.
    this.cam = { yaw: 0, pitch: -0.06, dist: 0, pos: [0, 0, 0] };
    this.pointers = [];
    this.status = "";
    this.sayings = [];
    this.resize();
    window.addEventListener("resize", () => this.resize());
    this.wireDesktop();
  }

  resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.canvas.width = Math.floor(this.canvas.clientWidth * dpr);
    this.canvas.height = Math.floor(this.canvas.clientHeight * dpr);
  }

  async load() {
    if (DEMO) {
      this.snap = demoSnapshot();
      this.scene.build(this.snap);
      this.say("demo fleet");
      return;
    }
    try {
      this.snap = await api("/v1/dashboard/state?session=*");
      this.scene.build(this.snap);
      const n = this.scene.nodes.length;
      this.say(`${n} agent${n === 1 ? "" : "s"}`);
    } catch (e) {
      // A hub you have not signed in to is the ordinary case on a headset:
      // say so, and show something rather than an empty room.
      this.snap = demoSnapshot();
      this.scene.build(this.snap);
      this.say("not signed in: showing a demo fleet");
    }
  }

  say(text) {
    this.status = text;
    const el = document.getElementById("status");
    if (el) el.textContent = text;
  }

  // ------------------------------------------------------- desktop input --
  wireDesktop() {
    const c = this.canvas;
    let down = false, lx = 0, ly = 0;
    c.addEventListener("pointerdown", (e) => {
      down = true; lx = e.clientX; ly = e.clientY;
      c.setPointerCapture(e.pointerId);
    });
    c.addEventListener("pointerup", (e) => {
      down = false;
      try { c.releasePointerCapture(e.pointerId); } catch (err) {}
      // A click that did not drag is a selection.
      if (Math.abs(e.clientX - lx) < 4 && Math.abs(e.clientY - ly) < 4) {
        if (this.scene.hovered) this.select(this.scene.hovered);
      }
    });
    c.addEventListener("pointermove", (e) => {
      if (down) {
        this.cam.yaw -= (e.clientX - lx) * 0.005;
        this.cam.pitch = Math.max(-1.2, Math.min(1.2,
          this.cam.pitch - (e.clientY - ly) * 0.005));
        lx = e.clientX; ly = e.clientY;
      }
      const rect = c.getBoundingClientRect();
      this.mouse = [
        ((e.clientX - rect.left) / rect.width) * 2 - 1,
        1 - ((e.clientY - rect.top) / rect.height) * 2,
      ];
    });
    c.addEventListener("wheel", (e) => {
      this.cam.dist = Math.max(-1.5, Math.min(4, this.cam.dist + Math.sign(e.deltaY) * 0.2));
      e.preventDefault();
    }, { passive: false });
    window.addEventListener("keydown", (e) => {
      if (e.key === "g") this.setMode(this.scene.mode === "graph" ? "team" : "graph");
      if (e.key === "v") this.listen();
    });
  }

  setMode(mode) {
    this.scene.mode = mode;
    this.scene.selected = null;
    this.say(mode === "graph" ? "the code graph" : "the team");
    const b = document.getElementById("modeBtn");
    if (b) b.textContent = mode === "graph" ? "the team" : "the code graph";
  }

  select(o) {
    this.scene.selected = o;
    if (!o) return;
    if (o.kind === "agent") {
      const a = o.data;
      this.say(`${a.name}: ${a.role || a.cli}, ${a.task_state || a.status}` +
               (a.host && a.host !== "-" ? `, on ${a.host}` : ""));
    } else {
      this.say(`${o.label}: ${o.sub}`);
    }
  }

  // ------------------------------------------------------------- the eye --
  desktopView() {
    // Standing in the room rather than looking down on it: eye height, a
    // step back from the arc, and the floor taking the bottom third.
    const d = 2.2 + this.cam.dist;
    const eye = [
      Math.sin(this.cam.yaw) * d,
      0.30 - this.cam.pitch * 1.6,
      Math.cos(this.cam.yaw) * d - 0.2,
    ];
    const at = [0, 0.16, -2.0];
    return { eye, view: M4.lookAt(eye, at, [0, 1, 0]) };
  }

  frame(now, xrFrame) {
    const gl = this.r.gl;
    if (xrFrame) {
      const pose = xrFrame.getViewerPose(this.refSpace);
      if (!pose) return;
      const layer = this.xr.renderState.baseLayer;
      gl.bindFramebuffer(gl.FRAMEBUFFER, layer.framebuffer);
      gl.clearColor(0.035, 0.04, 0.06, 1);
      gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
      this.updatePointers(xrFrame);
      for (const view of pose.views) {
        const vp = layer.getViewport(view);
        this.r.begin(vp, false);
        const eye = [
          view.transform.position.x, view.transform.position.y, view.transform.position.z];
        this.scene.render(view.projectionMatrix, view.transform.inverse.matrix, eye, now);
        this.renderPointers(view.projectionMatrix, view.transform.inverse.matrix);
        this.renderPanel(view.projectionMatrix, view.transform.inverse.matrix, eye);
      }
      return;
    }
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    const { eye, view } = this.desktopView();
    const proj = M4.perspective(
      Math.PI / 3, this.canvas.width / Math.max(this.canvas.height, 1), 0.05, 100);
    this.r.begin({ x: 0, y: 0, width: this.canvas.width, height: this.canvas.height }, true);
    // The mouse is a pointer too, so hovering reads the same as pointing.
    if (this.mouse) {
      const inv = M4.invert(view);
      const tanF = Math.tan(Math.PI / 6);
      const aspect = this.canvas.width / Math.max(this.canvas.height, 1);
      const dir = V3.norm(M4.dir(inv, [this.mouse[0] * tanF * aspect, this.mouse[1] * tanF, -1]));
      const hit = this.scene.hit(eye, dir);
      this.scene.hovered = hit ? hit.object : null;
      this.canvas.style.cursor = hit ? "pointer" : "grab";
    }
    this.scene.render(proj, view, eye, now);
    this.renderPanel(proj, view, eye);
  }

  // Controllers, and what they are pointing at.
  updatePointers(xrFrame) {
    this.pointers = [];
    const session = this.xr;
    if (!session) return;
    for (const src of session.inputSources) {
      if (!src.targetRaySpace) continue;
      const pose = xrFrame.getPose(src.targetRaySpace, this.refSpace);
      if (!pose) continue;
      const m = pose.transform.matrix;
      const origin = [m[12], m[13], m[14]];
      const dir = V3.norm(M4.dir(m, [0, 0, -1]));
      const hit = this.scene.hit(origin, dir);
      this.pointers.push({ origin, dir, hit, src });
      if (hit) this.scene.hovered = hit.object;
    }
    if (!this.pointers.some((p) => p.hit)) this.scene.hovered = null;
  }

  renderPointers(proj, view) {
    for (const p of this.pointers) {
      const len = p.hit ? p.hit.t : 6;
      const mid = V3.add(p.origin, V3.mul(p.dir, len / 2));
      // A thin box is a ray you can see, without a line primitive and the
      // state changes that come with one.
      const yaw = Math.atan2(p.dir[0], p.dir[2]);
      const pitch = Math.asin(Math.max(-1, Math.min(1, -p.dir[1])));
      const m = M4.mul(
        M4.mul(M4.mul(M4.translate(mid[0], mid[1], mid[2]), M4.rotY(yaw)), M4.rotX(-pitch)),
        M4.scale(0.006, 0.006, len));
      this.r.draw("box", m, p.hit ? [.55, .85, 1] : [.3, .36, .46], p.hit ? .5 : 0, proj, view);
    }
  }

  // One panel, in front of you, saying what is selected and what the keys
  // are. In a headset there is nowhere else to put it.
  renderPanel(proj, view, eye) {
    const text = (this.status || "nexmesh") + "\n" +
      (this.scene.mode === "graph" ? "pointing: a subsystem" : "pointing: a teammate") +
      "   ·   trigger selects   ·   squeeze: team / graph";
    const lab = this.scene.labelFor(text, { key: "panel:" + text, size: 34, fg: "#cfe0f5" });
    const h = 0.18, w = h * lab.aspect;
    const ahead = [0, -0.82, -1.5];
    this.r.drawLabel(lab, M4.mul(billboard(ahead, eye), M4.scale(w, h, 1)), proj, view, .92);
  }
}

// ------------------------------------------------------------ voice in XR --
// The same ears the dashboard uses, reached the same way: record, ask the
// fleet to transcribe, act on the words. In a headset this is the only
// comfortable input there is -- there is no keyboard, and pointing at a
// letter grid is not talking to your team.

App.prototype.listen = async function () {
  if (this.recording) {
    try { this.recording.stop(); } catch (e) {}
    return;
  }
  if (!(navigator.mediaDevices && window.MediaRecorder)) {
    this.say("no microphone here");
    return;
  }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (e) {
    this.say("no microphone: " + (e.message || e.name));
    return;
  }
  const chunks = [];
  const rec = new MediaRecorder(stream);
  this.recording = rec;
  this.say("listening…");
  rec.ondataavailable = (e) => chunks.push(e.data);
  rec.onstop = async () => {
    stream.getTracks().forEach((t) => t.stop());
    this.recording = null;
    const blob = new Blob(chunks, { type: chunks[0] ? chunks[0].type : "audio/webm" });
    if (blob.size < 1200) { this.say("nothing heard"); return; }
    this.say("transcribing…");
    try {
      const b64 = await new Promise((res, rej) => {
        const fr = new FileReader();
        fr.onload = () => res(String(fr.result).split(",")[1]);
        fr.onerror = rej;
        fr.readAsDataURL(blob);
      });
      const started = await api("/v1/capabilities/ask", {
        capability: "transcribe",
        request: { audio: `data:${blob.type};base64,${b64}` },
      });
      const id = started.ask_id;
      let text = "";
      for (let i = 0; i < 60 && !text; i++) {
        await new Promise((r) => setTimeout(r, 700));
        const v = await api(`/v1/capabilities/ask/${id}`);
        if (v.done) {
          try {
            const j = JSON.parse(v.result);
            text = (j.text || j.data?.text || v.result || "").trim();
          } catch (e) { text = String(v.result || "").trim(); }
          break;
        }
      }
      if (!text) { this.say("nothing came back"); return; }
      this.heard(text);
    } catch (e) {
      this.say("could not transcribe: " + (e.message || e));
    }
  };
  rec.start();
  // A sentence, not a monologue: the headset has no second hand to press
  // stop with while you are pointing at something.
  setTimeout(() => { if (this.recording === rec) { try { rec.stop(); } catch (e) {} } }, 6000);
};

// What a spoken sentence does. Selection by voice is the point: in a
// headset, naming a teammate is quicker than pointing at one, and both
// should get you to the same place.
App.prototype.heard = function (text) {
  const said = text.trim();
  const low = said.toLowerCase().replace(/[.,!?]+$/, "");
  if (/^(show|open|go to|the)?\s*(code )?graph$/.test(low)) { this.setMode("graph"); return; }
  if (/^(show|open|go to|the)?\s*(team|agents|room)$/.test(low)) { this.setMode("team"); return; }
  // "select Iris", "Iris", "talk to Iris": whichever, find the teammate.
  const name = low.replace(/^(select|choose|pick|show|open|talk to|message)\s+/, "");
  const found = this.scene.active().find((o) =>
    (o.label || "").toLowerCase() === name ||
    (o.data && (o.data.role || "").toLowerCase() === name));
  if (found) { this.select(found); return; }
  // Not a command and not a name: say it to whoever is selected, or to the
  // team. Talking to the room is the ordinary case.
  this.sendSaid(said);
};

App.prototype.sendSaid = async function (said) {
  const sel = this.scene.selected;
  const session = (sel && sel.data && sel.data.session) ||
    (this.scene.sessions && this.scene.sessions[0]) || "";
  if (!session) { this.say(`heard: ${said}`); return; }
  try {
    const body = { text: said };
    if (sel && sel.kind === "agent") body.to = sel.data.name;
    await api(`/v1/sessions/${encodeURIComponent(session)}/messages`, body);
    this.say(`sent to ${body.to || "the team"}: ${said}`);
  } catch (e) {
    this.say("could not send that: " + (e.message || e));
  }
};

// --------------------------------------------------------------- session --

App.prototype.enterXR = async function () {
  if (!navigator.xr) { this.say("this browser has no WebXR"); return; }
  const ok = await navigator.xr.isSessionSupported("immersive-vr").catch(() => false);
  if (!ok) { this.say("no immersive headset here"); return; }
  const session = await navigator.xr.requestSession("immersive-vr", {
    optionalFeatures: ["local-floor", "bounded-floor", "hand-tracking", "layers"],
  });
  this.xr = session;
  const gl = this.r.gl;
  await gl.makeXRCompatible();
  session.updateRenderState({ baseLayer: new XRWebGLLayer(session, gl) });
  this.refSpace = await session.requestReferenceSpace("local-floor")
    .catch(() => session.requestReferenceSpace("local"));
  session.addEventListener("end", () => {
    this.xr = null;
    this.say("left the headset");
    requestAnimationFrame((t) => this.loop(t));
  });
  // The trigger selects what a controller is pointing at; the grip swaps
  // between the team and the map, because that is the other thing you do.
  session.addEventListener("select", () => {
    const p = this.pointers.find((x) => x.hit);
    if (p) this.select(p.hit.object);
    else this.listen();
  });
  session.addEventListener("squeeze", () => {
    this.setMode(this.scene.mode === "graph" ? "team" : "graph");
  });
  this.say("in the headset");
  session.requestAnimationFrame((t, f) => this.xrLoop(t, f));
};

App.prototype.xrLoop = function (t, f) {
  if (!this.xr) return;
  this.frame(t, f);
  this.xr.requestAnimationFrame((tt, ff) => this.xrLoop(tt, ff));
};

App.prototype.loop = function (t) {
  if (this.xr) return;               // the headset drives its own frames
  this.frame(t, null);
  requestAnimationFrame((tt) => this.loop(tt));
};

// A camera that walks itself, for recording what this looks like. Turned on
// with ?tour=1, which is what the README's animation is made from.
App.prototype.tour = function () {
  const legs = [
    { mode: "team", from: -0.75, to: 0.75, ms: 7000, select: 2 },
    { mode: "graph", from: 0.6, to: -0.6, ms: 6000, select: 3 },
  ];
  let i = 0, start = performance.now();
  const step = (now) => {
    const leg = legs[i % legs.length];
    const k = Math.min(1, (now - start) / leg.ms);
    // Ease, so the motion reads as a camera rather than a slider.
    const e = k < .5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;
    if (this.scene.mode !== leg.mode) this.setMode(leg.mode);
    this.cam.yaw = leg.from + (leg.to - leg.from) * e;
    this.cam.pitch = -0.05 + Math.sin(k * Math.PI) * 0.08;
    if (k > .45 && this.scene.selected === null) {
      const list = this.scene.active();
      this.select(list[Math.min(leg.select, list.length - 1)]);
    }
    if (k >= 1) { i++; start = now; this.scene.selected = null; }
    requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
};

window.addEventListener("load", async () => {
  let app;
  try {
    app = new App();
  } catch (e) {
    document.getElementById("status").textContent = String(e.message || e);
    return;
  }
  window.nexmeshXR = app;             // the capture script drives this
  await app.load();
  document.getElementById("enterBtn").onclick = () => app.enterXR();
  document.getElementById("modeBtn").onclick = () =>
    app.setMode(app.scene.mode === "graph" ? "team" : "graph");
  document.getElementById("micBtn").onclick = () => app.listen();
  if (navigator.xr) {
    navigator.xr.isSessionSupported("immersive-vr").then((ok) => {
      const b = document.getElementById("enterBtn");
      if (!ok) { b.disabled = true; b.textContent = "no headset here"; }
    }).catch(() => {});
  } else {
    const b = document.getElementById("enterBtn");
    b.disabled = true;
    b.textContent = "no WebXR here";
  }
  if (!DEMO) setInterval(() => app.load().catch(() => {}), 4000);
  app.loop(performance.now());
  if (params.get("tour") === "1") app.tour();
});
