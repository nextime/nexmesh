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
// internet, and a headset that cannot load a bundle shows nothing.
//
// What is in here, in the order it appears:
//
//   maths        4x4 matrices, the layout WebGL wants
//   geometry     lofted surfaces: every body part is a stack of superellipse
//                cross-sections with smooth normals, authored in metres
//   renderer     physically based shading, a shadow map, environment
//                reflections, bump-mapped materials, a sky and a floor
//   figures      a jointed character per teammate, posed every frame
//   scene        where everything stands, and the code graph
//   panels       terminals, the chat and the menu, as screens in the room
//   app          input, the headset session, voice, and a scripted camera
//
// Two modes, because a headset is two different tools: the ordinary
// dashboard in a floating window, which is `/`, and this -- immersive, the
// team standing around you. Without a headset the same scene renders to
// the canvas with a mouse, which is what the pictures in the README are.

"use strict";

// ----------------------------------------------------------------- maths --
// Column-major 4x4. The floor is y = 0 and a metre is a metre, which is
// what a headset's `local-floor` space means and so what everything here
// is authored in.

const M4 = {
  ident: () => new Float32Array([1,0,0,0, 0,1,0,0, 0,0,1,0, 0,0,0,1]),
  mul(a, b) {
    const o = new Float32Array(16);
    for (let c = 0; c < 4; c++) {
      const b0 = b[c*4], b1 = b[c*4+1], b2 = b[c*4+2], b3 = b[c*4+3];
      o[c*4]   = a[0]*b0 + a[4]*b1 + a[8]*b2  + a[12]*b3;
      o[c*4+1] = a[1]*b0 + a[5]*b1 + a[9]*b2  + a[13]*b3;
      o[c*4+2] = a[2]*b0 + a[6]*b1 + a[10]*b2 + a[14]*b3;
      o[c*4+3] = a[3]*b0 + a[7]*b1 + a[11]*b2 + a[15]*b3;
    }
    return o;
  },
  translate(x, y, z) { const m = M4.ident(); m[12] = x; m[13] = y; m[14] = z; return m; },
  scale(x, y, z) { const m = M4.ident(); m[0] = x; m[5] = y; m[10] = z; return m; },
  rotX(a) {
    const m = M4.ident(), c = Math.cos(a), s = Math.sin(a);
    m[5] = c; m[6] = s; m[9] = -s; m[10] = c;
    return m;
  },
  rotY(a) {
    const m = M4.ident(), c = Math.cos(a), s = Math.sin(a);
    m[0] = c; m[2] = -s; m[8] = s; m[10] = c;
    return m;
  },
  rotZ(a) {
    const m = M4.ident(), c = Math.cos(a), s = Math.sin(a);
    m[0] = c; m[1] = s; m[4] = -s; m[5] = c;
    return m;
  },
  perspective(fovy, aspect, near, far) {
    const f = 1 / Math.tan(fovy / 2), m = new Float32Array(16);
    m[0] = f / aspect; m[5] = f; m[11] = -1;
    m[10] = (far + near) / (near - far);
    m[14] = (2 * far * near) / (near - far);
    return m;
  },
  ortho(l, r, b, t, n, f) {
    const m = M4.ident();
    m[0] = 2 / (r - l); m[5] = 2 / (t - b); m[10] = -2 / (f - n);
    m[12] = -(r + l) / (r - l); m[13] = -(t + b) / (t - b); m[14] = -(f + n) / (f - n);
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
  norm(a) { const l = V3.len(a) || 1; return [a[0]/l, a[1]/l, a[2]/l]; },
  lerp: (a, b, t) => [a[0]+(b[0]-a[0])*t, a[1]+(b[1]-a[1])*t, a[2]+(b[2]-a[2])*t],
};

// A chain of transforms, read left to right: parent first.
function xf() {
  let m = arguments[0];
  for (let i = 1; i < arguments.length; i++) m = M4.mul(m, arguments[i]);
  return m;
}
const T = M4.translate, S = M4.scale, RX = M4.rotX, RY = M4.rotY, RZ = M4.rotZ;

// Put a thing at `from` with its +Y axis pointing at `to`. Limbs, rays and
// the edges of the graph are all "a shape between two points".
function alignY(from, to) {
  const y = V3.norm(V3.sub(to, from));
  const ref = Math.abs(y[1]) < 0.99 ? [0, 1, 0] : [1, 0, 0];
  const x = V3.norm(V3.cross(ref, y));
  const z = V3.cross(x, y);
  return new Float32Array([
    x[0], x[1], x[2], 0,
    y[0], y[1], y[2], 0,
    z[0], z[1], z[2], 0,
    from[0], from[1], from[2], 1,
  ]);
}

function hashStr(str) {
  let h = 2166136261;
  for (let i = 0; i < str.length; i++) h = Math.imul(h ^ str.charCodeAt(i), 16777619);
  return h >>> 0;
}
const clamp = (v, a, b) => Math.max(a, Math.min(b, v));
const smooth = (k) => k < .5 ? 2 * k * k : 1 - Math.pow(-2 * k + 2, 2) / 2;

// -------------------------------------------------------------- geometry --
// One primitive does nearly everything: a LOFT. A stack of cross-sections
// along Y, each a superellipse with its own radii, centre and squareness,
// joined into a surface with normals taken from the surface itself. A
// thigh, a ribcage, a boot and a helmet are all "how wide, how deep, how
// square, at each height" -- which is how a sculptor blocks out a figure,
// and it gives smooth shading for free where boxes gave facets.

function pushTri(out, pa, na, pb, nb, pc, nc) {
  const fn = V3.cross(V3.sub(pb, pa), V3.sub(pc, pa));
  if (V3.len(fn) < 1e-12) return;
  const sum = [na[0]+nb[0]+nc[0], na[1]+nb[1]+nc[1], na[2]+nb[2]+nc[2]];
  if (V3.dot(fn, sum) < 0) {
    out.push(pa[0],pa[1],pa[2], na[0],na[1],na[2],
             pc[0],pc[1],pc[2], nc[0],nc[1],nc[2],
             pb[0],pb[1],pb[2], nb[0],nb[1],nb[2]);
  } else {
    out.push(pa[0],pa[1],pa[2], na[0],na[1],na[2],
             pb[0],pb[1],pb[2], nb[0],nb[1],nb[2],
             pc[0],pc[1],pc[2], nc[0],nc[1],nc[2]);
  }
}

// sections: [{ y, rx, rz?, ox?, oz?, e? }]. `e` is the squareness: 2 is an
// ellipse, 4 is a rounded box, and the values between are where bodies and
// armour actually live.
function loft(sections, seg, opts) {
  const o = opts || {};
  const n = sections.length;
  const rings = sections.map((s) => {
    const e = s.e || o.e || 2;
    const rz = s.rz === undefined ? s.rx : s.rz;
    const pts = [];
    for (let j = 0; j < seg; j++) {
      const a = (j / seg) * Math.PI * 2;
      const c = Math.cos(a), sn = Math.sin(a);
      const px = Math.sign(c) * Math.pow(Math.abs(c), 2 / e);
      const pz = Math.sign(sn) * Math.pow(Math.abs(sn), 2 / e);
      pts.push([(s.ox || 0) + px * s.rx, s.y, (s.oz || 0) + pz * rz]);
    }
    return pts;
  });
  const normals = rings.map((ring, i) => ring.map((p, j) => {
    const s = sections[i];
    const radial = [p[0] - (s.ox || 0), 0, p[2] - (s.oz || 0)];
    const tr = V3.sub(ring[(j + 1) % seg], ring[(j - 1 + seg) % seg]);
    const ts = V3.sub(rings[Math.min(i + 1, n - 1)][j], rings[Math.max(i - 1, 0)][j]);
    let nn = V3.cross(tr, ts);
    if (V3.len(nn) < 1e-10 || V3.len(radial) < 1e-7) {
      // A pole: the surface closes here, and its normal is along the spine.
      const other = i === 0 ? sections[1] : sections[i - 1];
      return [0, s.y >= other.y ? 1 : -1, 0];
    }
    nn = V3.norm(nn);
    return V3.dot(nn, radial) < 0 ? V3.mul(nn, -1) : nn;
  }));
  const out = [];
  for (let i = 0; i < n - 1; i++) {
    for (let j = 0; j < seg; j++) {
      const j2 = (j + 1) % seg;
      const a = rings[i][j], b = rings[i][j2], c = rings[i + 1][j2], d = rings[i + 1][j];
      const na = normals[i][j], nb = normals[i][j2], nc = normals[i + 1][j2],
            nd = normals[i + 1][j];
      pushTri(out, a, na, b, nb, c, nc);
      pushTri(out, a, na, c, nc, d, nd);
    }
  }
  const cap = (i, ny) => {
    const s = sections[i];
    const centre = [s.ox || 0, s.y, s.oz || 0];
    const nrm = [0, ny, 0];
    for (let j = 0; j < seg; j++) {
      pushTri(out, centre, nrm, rings[i][j], nrm, rings[i][(j + 1) % seg], nrm);
    }
  };
  if (o.capBottom) cap(0, sections[0].y <= sections[n - 1].y ? -1 : 1);
  if (o.capTop) cap(n - 1, sections[n - 1].y >= sections[0].y ? 1 : -1);
  return new Float32Array(out);
}

// A sphere is a loft whose radius follows a circle.
function sphereMesh(seg, rings) {
  const sec = [];
  for (let i = 0; i <= rings; i++) {
    const a = -Math.PI / 2 + (i / rings) * Math.PI;
    sec.push({ y: Math.sin(a) * 0.5, rx: Math.max(Math.cos(a) * 0.5, 0) });
  }
  return loft(sec, seg);
}

function torusMesh(R, r, segR, segr) {
  const out = [];
  const pt = (i, j) => {
    const a = (i / segR) * Math.PI * 2, b = (j / segr) * Math.PI * 2;
    const cx = Math.cos(a), cz = Math.sin(a);
    const p = [(R + r * Math.cos(b)) * cx, r * Math.sin(b), (R + r * Math.cos(b)) * cz];
    const nn = [Math.cos(b) * cx, Math.sin(b), Math.cos(b) * cz];
    return [p, nn];
  };
  for (let i = 0; i < segR; i++) {
    for (let j = 0; j < segr; j++) {
      const [a, na] = pt(i, j), [b, nb] = pt(i + 1, j), [c, nc] = pt(i + 1, j + 1),
            [d, nd] = pt(i, j + 1);
      pushTri(out, a, na, b, nb, c, nc);
      pushTri(out, a, na, c, nc, d, nd);
    }
  }
  return new Float32Array(out);
}

// Flat-shaded solids, for the things that are meant to look cut: the badge
// over a teammate's head is a hologram of a crystal, not a body part.
function flatMesh(name) {
  const tri = (a, b, c) => {
    const nn = V3.norm(V3.cross(V3.sub(b, a), V3.sub(c, a)));
    return [...a, ...nn, ...b, ...nn, ...c, ...nn];
  };
  const quad = (a, b, c, d) => [...tri(a, b, c), ...tri(a, c, d)];
  const out = [];
  if (name === "cube") {
    const p = [[-.5,-.5,-.5],[.5,-.5,-.5],[.5,.5,-.5],[-.5,.5,-.5],
               [-.5,-.5,.5],[.5,-.5,.5],[.5,.5,.5],[-.5,.5,.5]];
    out.push(...quad(p[4],p[5],p[6],p[7]), ...quad(p[1],p[0],p[3],p[2]),
             ...quad(p[0],p[4],p[7],p[3]), ...quad(p[5],p[1],p[2],p[6]),
             ...quad(p[3],p[7],p[6],p[2]), ...quad(p[0],p[1],p[5],p[4]));
  } else if (name === "octa" || name === "icosa") {
    let faces = [
      [[1,0,0],[0,1,0],[0,0,1]], [[0,1,0],[-1,0,0],[0,0,1]],
      [[-1,0,0],[0,-1,0],[0,0,1]], [[0,-1,0],[1,0,0],[0,0,1]],
      [[0,1,0],[1,0,0],[0,0,-1]], [[-1,0,0],[0,1,0],[0,0,-1]],
      [[0,-1,0],[-1,0,0],[0,0,-1]], [[1,0,0],[0,-1,0],[0,0,-1]],
    ];
    if (name === "icosa") {
      const next = [];
      for (const [a, b, c] of faces) {
        const ab = V3.norm(V3.add(a, b)), bc = V3.norm(V3.add(b, c)),
              ca = V3.norm(V3.add(c, a));
        next.push([a, ab, ca], [ab, b, bc], [ca, bc, c], [ab, bc, ca]);
      }
      faces = next;
    }
    for (const [a, b, c] of faces) {
      out.push(...tri(V3.mul(a, .5), V3.mul(b, .5), V3.mul(c, .5)));
    }
  } else if (name === "tetra") {
    const p = [[0,.6,0],[-.5,-.3,.4],[.5,-.3,.4],[0,-.3,-.6]];
    out.push(...tri(p[0],p[1],p[2]), ...tri(p[0],p[2],p[3]),
             ...tri(p[0],p[3],p[1]), ...tri(p[1],p[3],p[2]));
  } else if (name === "quad") {
    out.push(...quad([-.5,-.5,0],[.5,-.5,0],[.5,.5,0],[-.5,.5,0]));
  }
  return new Float32Array(out);
}

// ---------------------------------------------------- the authored parts --
// A figure, in metres, standing on y = 0. Each part hangs from its joint:
// limbs run down -Y from the joint they turn about, the torso stacks up +Y.
// The numbers are the sculpt -- chest wider at the shoulder line than the
// waist, a calf that is fuller behind than in front, a forearm that tapers
// to the wrist -- and changing one is how a silhouette is changed.

function bodyMeshes() {
  const SEG = 28;
  const M = {};
  // Unit helpers.
  M.sph = sphereMesh(28, 18);
  M.tube = loft([{ y: -.5, rx: .5 }, { y: .5, rx: .5 }], 28);
  M.cyl = loft([{ y: -.5, rx: .5 }, { y: .5, rx: .5 }], 32, { capBottom: true, capTop: true });
  M.rbox = loft([
    { y: -.5, rx: .40, e: 6 }, { y: -.44, rx: .5, e: 6 },
    { y: .44, rx: .5, e: 6 }, { y: .5, rx: .40, e: 6 },
  ], 32, { capBottom: true, capTop: true });
  M.torus = torusMesh(.5, .018, 64, 10);
  M.torusFat = torusMesh(.5, .05, 48, 12);
  for (const f of ["cube", "octa", "icosa", "tetra", "quad"]) M[f] = flatMesh(f);

  // Torso, bottom to top.
  M.pelvis = loft([
    { y: -.135, rx: .045, rz: .05, e: 2.2 }, { y: -.10, rx: .112, rz: .088, e: 2.3 },
    { y: -.05, rx: .150, rz: .106, e: 2.4 }, { y: 0, rx: .158, rz: .110, e: 2.5 },
    { y: .06, rx: .148, rz: .102, e: 2.5 }, { y: .10, rx: .139, rz: .098, e: 2.4 },
  ], SEG, { capBottom: true });
  M.belt = loft([
    { y: .045, rx: .150, rz: .104, e: 2.6 }, { y: .052, rx: .158, rz: .112, e: 2.6 },
    { y: .092, rx: .152, rz: .108, e: 2.6 }, { y: .10, rx: .144, rz: .101, e: 2.6 },
  ], SEG);
  M.abdomen = loft([
    { y: -.02, rx: .140, rz: .098, e: 2.4 }, { y: .03, rx: .129, rz: .092, e: 2.3 },
    { y: .08, rx: .126, rz: .091, e: 2.3 }, { y: .13, rx: .134, rz: .099, e: 2.4 },
  ], SEG);
  M.chest = loft([
    { y: 0, rx: .134, rz: .099, e: 2.4 }, { y: .05, rx: .150, rz: .108, oz: .003, e: 2.4 },
    { y: .12, rx: .174, rz: .119, oz: .008, e: 2.5 },
    { y: .19, rx: .192, rz: .122, oz: .010, e: 2.6 },
    { y: .25, rx: .198, rz: .110, oz: .005, e: 2.6 },
    { y: .285, rx: .160, rz: .092, e: 2.4 }, { y: .308, rx: .085, rz: .066, e: 2.2 },
    { y: .322, rx: .050, rz: .048, e: 2 },
  ], SEG);
  // The plate over the ribcage: the same shape, proud of it, and squarer.
  M.chestArmor = loft([
    { y: .085, rx: .150, rz: .100, oz: .020, e: 3.2 },
    { y: .10, rx: .178, rz: .124, oz: .020, e: 3.2 },
    { y: .17, rx: .202, rz: .134, oz: .020, e: 3.4 },
    { y: .245, rx: .208, rz: .124, oz: .014, e: 3.4 },
    { y: .268, rx: .178, rz: .100, oz: .008, e: 3 },
  ], SEG);
  M.collar = loft([
    { y: .268, rx: .092, rz: .082, oz: -.004, e: 2.4 },
    { y: .30, rx: .074, rz: .070, oz: -.004, e: 2.2 },
    { y: .345, rx: .064, rz: .064, oz: -.008, e: 2.2 },
    { y: .35, rx: .056, rz: .056, oz: -.008, e: 2.2 },
  ], SEG);
  M.neck = loft([
    { y: -.02, rx: .050, rz: .052 }, { y: .03, rx: .046, rz: .050 }, { y: .075, rx: .047, rz: .052 },
  ], 20);

  // The head: a skull with a jaw, not a ball.
  M.skull = loft([
    { y: -.012, rx: .044, rz: .050, oz: .002 },
    { y: .028, rx: .066, rz: .078, oz: .010, e: 2.2 },
    { y: .075, rx: .081, rz: .096, oz: .007, e: 2.2 },
    { y: .13, rx: .088, rz: .104, oz: 0, e: 2.2 },
    { y: .18, rx: .086, rz: .101, oz: -.004, e: 2.1 },
    { y: .222, rx: .068, rz: .080, oz: -.006 },
    { y: .246, rx: .034, rz: .040, oz: -.006 },
    { y: .254, rx: .0005, rz: .0005, oz: -.006 },
  ], SEG);
  M.visor = loft([
    { y: .082, rx: .078, rz: .088, oz: .020, e: 2.5 },
    { y: .098, rx: .092, rz: .104, oz: .022, e: 2.5 },
    { y: .152, rx: .094, rz: .105, oz: .020, e: 2.5 },
    { y: .168, rx: .084, rz: .094, oz: .015, e: 2.5 },
  ], SEG);
  M.jaw = loft([
    { y: -.006, rx: .050, rz: .058, oz: .012, e: 2.6 },
    { y: .012, rx: .072, rz: .086, oz: .016, e: 2.6 },
    { y: .058, rx: .086, rz: .100, oz: .014, e: 2.6 },
    { y: .072, rx: .082, rz: .094, oz: .010, e: 2.6 },
  ], SEG);
  M.wing = loft([
    { y: 0, rx: .007, rz: .034, oz: 0, e: 2 }, { y: .04, rx: .008, rz: .046, oz: -.016, e: 2 },
    { y: .10, rx: .006, rz: .034, oz: -.046, e: 2 }, { y: .15, rx: .003, rz: .014, oz: -.074, e: 2 },
    { y: .165, rx: .0005, rz: .0005, oz: -.082, e: 2 },
  ], 14);

  // Arms.
  M.pad = loft([
    { y: -.055, rx: .074, rz: .080, e: 2.8 }, { y: -.02, rx: .082, rz: .088, e: 2.8 },
    { y: .025, rx: .076, rz: .082, e: 2.6 }, { y: .052, rx: .052, rz: .058, e: 2.3 },
    { y: .066, rx: .0005, rz: .0005 },
  ], 24);
  M.upperArm = loft([
    { y: .03, rx: .030, rz: .030 }, { y: 0, rx: .056, rz: .058 },
    { y: -.07, rx: .056, rz: .060, oz: .002 }, { y: -.15, rx: .051, rz: .053 },
    { y: -.24, rx: .043, rz: .044 }, { y: -.295, rx: .040, rz: .041 },
  ], 22);
  M.forearm = loft([
    { y: .01, rx: .040, rz: .040 }, { y: -.05, rx: .047, rz: .048 },
    { y: -.12, rx: .043, rz: .042 }, { y: -.20, rx: .034, rz: .032 },
    { y: -.268, rx: .030, rz: .026 },
  ], 22);
  M.bracer = loft([
    { y: -.045, rx: .049, rz: .050, e: 2.6 }, { y: -.055, rx: .056, rz: .057, e: 3 },
    { y: -.13, rx: .052, rz: .051, e: 3 }, { y: -.205, rx: .043, rz: .041, e: 3 },
    { y: -.215, rx: .036, rz: .034, e: 2.6 },
  ], 22);
  M.palm = loft([
    { y: .004, rx: .029, rz: .022, e: 2.4 }, { y: -.03, rx: .043, rz: .019, e: 2.8 },
    { y: -.085, rx: .041, rz: .015, e: 3 }, { y: -.102, rx: .034, rz: .011, e: 3 },
  ], 18, { capTop: true });
  M.fingers = loft([
    { y: .004, rx: .037, rz: .012, e: 3.2 }, { y: -.04, rx: .034, rz: .011, e: 3.2 },
    { y: -.072, rx: .028, rz: .009, e: 3 }, { y: -.082, rx: .016, rz: .005, e: 2.4 },
  ], 16, { capTop: true });
  M.finger = loft([
    { y: .003, rx: .0088, rz: .0082 }, { y: -.02, rx: .0085, rz: .008 },
    { y: -.04, rx: .0075, rz: .007 }, { y: -.046, rx: .004, rz: .004 },
    { y: -.048, rx: .0004, rz: .0004 },
  ], 10);
  M.thumb = loft([
    { y: .004, rx: .012, rz: .011 }, { y: -.03, rx: .0115, rz: .0105 },
    { y: -.052, rx: .009, rz: .0085 }, { y: -.06, rx: .0004, rz: .0004 },
  ], 10);

  // Legs.
  M.thigh = loft([
    { y: .03, rx: .060, rz: .064 }, { y: -.04, rx: .083, rz: .088 },
    { y: -.14, rx: .079, rz: .084, oz: .004 }, { y: -.28, rx: .064, rz: .067 },
    { y: -.40, rx: .053, rz: .056 }, { y: -.435, rx: .050, rz: .053 },
  ], 24);
  M.thighPlate = loft([
    { y: -.07, rx: .078, rz: .080, oz: .010, e: 2.8 }, { y: -.085, rx: .088, rz: .092, oz: .010, e: 3 },
    { y: -.20, rx: .078, rz: .082, oz: .010, e: 3 }, { y: -.30, rx: .066, rz: .068, oz: .008, e: 3 },
    { y: -.31, rx: .058, rz: .060, oz: .006, e: 2.8 },
  ], 24);
  M.kneeCap = loft([
    { y: -.055, rx: .034, rz: .020, oz: .040, e: 2.6 }, { y: -.03, rx: .052, rz: .034, oz: .036, e: 2.8 },
    { y: .025, rx: .054, rz: .036, oz: .034, e: 2.8 }, { y: .055, rx: .036, rz: .022, oz: .034, e: 2.6 },
    { y: .062, rx: .0005, rz: .0005, oz: .034 },
  ], 18, { capBottom: true });
  M.shin = loft([
    { y: .01, rx: .050, rz: .053 }, { y: -.08, rx: .055, rz: .061, oz: -.007 },
    { y: -.18, rx: .049, rz: .055, oz: -.006 }, { y: -.32, rx: .037, rz: .039 },
    { y: -.415, rx: .035, rz: .037 },
  ], 24);
  M.greave = loft([
    { y: -.055, rx: .052, rz: .054, oz: .008, e: 2.8 }, { y: -.07, rx: .060, rz: .064, oz: .008, e: 3.1 },
    { y: -.19, rx: .055, rz: .059, oz: .006, e: 3.1 }, { y: -.33, rx: .043, rz: .044, oz: .004, e: 3 },
    { y: -.345, rx: .037, rz: .038, oz: .003, e: 2.8 },
  ], 24);
  M.boot = loft([
    { y: -.092, rx: .050, rz: .128, oz: .046, e: 3.2 }, { y: -.078, rx: .056, rz: .134, oz: .046, e: 3.2 },
    { y: -.045, rx: .054, rz: .118, oz: .038, e: 3 }, { y: -.012, rx: .047, rz: .078, oz: .018, e: 2.6 },
    { y: .03, rx: .043, rz: .050, oz: .002, e: 2.4 }, { y: .085, rx: .041, rz: .044, e: 2.3 },
    { y: .095, rx: .037, rz: .040, e: 2.3 },
  ], 24, { capBottom: true });
  M.sole = loft([
    { y: -.108, rx: .054, rz: .134, oz: .046, e: 3.4 }, { y: -.088, rx: .058, rz: .138, oz: .046, e: 3.4 },
  ], 24, { capBottom: true });
  return M;
}

// --------------------------------------------------------------- renderer --
// One light, a shadow map, physically based surfaces, and a sky to reflect.
//
// The difference between "shapes" and "a place" is almost entirely here:
// things cast shadows on the floor and on each other, metal reflects the
// horizon, cloth has a weave you can see at arm's length, and the dark is a
// colour rather than black.

const LIGHT_DIR = V3.norm([0.42, 0.86, 0.36]);
const SHADOW_SIZE = 2048;

const VERT = `#version 300 es
layout(location=0) in vec3 pos;
layout(location=1) in vec3 nrm;
uniform mat4 uViewProj, uModel, uLightVP;
out vec3 vWorld;
out vec3 vNormal;
out vec3 vLocal;
out vec4 vShadow;
void main() {
  vec4 w = uModel * vec4(pos, 1.0);
  vWorld = w.xyz;
  vLocal = pos;
  // The inverse transpose, so a part that is stretched still shades as the
  // shape it has become rather than the one it was authored as.
  vNormal = transpose(inverse(mat3(uModel))) * nrm;
  vShadow = uLightVP * w;
  gl_Position = uViewProj * w;
}`;

// Shared by every lit surface: the sky as something to reflect, the shadow
// lookup, and the tone curve.
const COMMON = `
precision highp float;
precision lowp sampler2DShadow;
uniform vec3 uEye;
uniform sampler2DShadow uShadow;
uniform float uShadowOn;
const vec3 L = vec3(${LIGHT_DIR[0].toFixed(5)}, ${LIGHT_DIR[1].toFixed(5)}, ${LIGHT_DIR[2].toFixed(5)});
const vec3 FOG = vec3(0.030, 0.052, 0.090);

// What is overhead. Used for the background and, more importantly, for
// what a visor or a pauldron reflects: a zenith, a bright horizon band, and
// a soft box where the key light is, which is the highlight you read as
// "polished".
vec3 skyAt(vec3 d) {
  float h = d.y;
  vec3 zen = vec3(0.0030, 0.0045, 0.0105);
  vec3 hor = vec3(0.0130, 0.0240, 0.0470);
  vec3 c = mix(hor, zen, smoothstep(-0.05, 0.45, h));
  // A band where the room ends: enough to separate a dark figure from the
  // dark behind it, and no more.
  c += vec3(0.020, 0.050, 0.090) * exp(-abs(h) * 16.0);
  if (h < 0.0) c = mix(hor * 0.5, vec3(0.004, 0.006, 0.011), smoothstep(0.0, -0.3, h));
  return c;
}
vec3 envAt(vec3 d, float rough) {
  vec3 c = skyAt(d);
  float k = mix(220.0, 6.0, rough);
  c += vec3(1.0, 0.96, 0.90) * pow(max(dot(d, L), 0.0), k) * mix(5.0, 0.6, rough);
  // A second, cooler box behind: the rim that separates a dark suit from a
  // dark room.
  c += vec3(0.35, 0.55, 1.0) * pow(max(dot(d, normalize(vec3(-0.6, 0.3, -0.7))), 0.0),
                                    mix(60.0, 4.0, rough)) * 1.1;
  return c;
}

float shadowAt(vec4 sc, float ndl) {
  if (uShadowOn < 0.5) return 1.0;
  vec3 p = sc.xyz / sc.w * 0.5 + 0.5;
  if (p.x < 0.0 || p.x > 1.0 || p.y < 0.0 || p.y > 1.0 || p.z > 1.0) return 1.0;
  float bias = max(0.0035 * (1.0 - ndl), 0.0012);
  float t = 1.0 / ${SHADOW_SIZE.toFixed(1)};
  float s = 0.0;
  // Nine taps on hardware-filtered compares: a soft edge for the price of
  // a small blur.
  for (int x = -1; x <= 1; x++) {
    for (int y = -1; y <= 1; y++) {
      s += texture(uShadow, vec3(p.xy + vec2(float(x), float(y)) * t * 1.4, p.z - bias));
    }
  }
  return s / 9.0;
}

vec3 tone(vec3 c) {
  c = (c * (2.51 * c + 0.03)) / (c * (2.43 * c + 0.59) + 0.14);
  return pow(clamp(c, 0.0, 1.0), vec3(1.0 / 2.2));
}
`;

const FRAG = `#version 300 es
${COMMON}
in vec3 vNormal;
in vec3 vWorld;
in vec3 vLocal;
in vec4 vShadow;
uniform vec3 uColour;
// x roughness, y metalness, z emission, w highlight (the selection glow)
uniform vec4 uMat;
// x: surface detail. 0 smooth, 1 woven technical fabric, 2 armour with
// panel lines, 3 brushed metal.
uniform vec4 uMat2;
out vec4 frag;

float ggx(float ndh, float r) {
  float a = r * r, a2 = a * a;
  float d = ndh * ndh * (a2 - 1.0) + 1.0;
  return a2 / max(3.14159 * d * d, 1e-5);
}
float smithG(float ndv, float ndl, float r) {
  float k = (r + 1.0) * (r + 1.0) / 8.0;
  return (ndv / (ndv * (1.0 - k) + k)) * (ndl / (ndl * (1.0 - k) + k));
}
vec3 lightIt(vec3 n, vec3 v, vec3 l, vec3 lcol, vec3 albedo, float rough, float metal) {
  vec3 h = normalize(l + v);
  float ndl = max(dot(n, l), 0.0), ndv = max(dot(n, v), 1e-4);
  float ndh = max(dot(n, h), 0.0), vdh = max(dot(v, h), 0.0);
  vec3 f0 = mix(vec3(0.04), albedo, metal);
  vec3 fres = f0 + (1.0 - f0) * pow(1.0 - vdh, 5.0);
  vec3 spec = fres * ggx(ndh, rough) * smithG(ndv, ndl, rough) / (4.0 * ndv * ndl + 1e-4);
  vec3 diff = (1.0 - fres) * (1.0 - metal) * albedo / 3.14159;
  return (diff + spec) * lcol * ndl;
}

float hash3(vec3 p) { return fract(sin(dot(p, vec3(12.9898, 78.233, 37.719))) * 43758.5453); }

// The surface's own relief, as a height. Authored in the part's own
// coordinates, so a weave stays on the sleeve when the arm moves.
float relief(vec3 p, float kind, out float rmod) {
  rmod = 0.0;
  if (kind < 0.5) return 0.0;
  if (kind < 1.5) {
    // Technical fabric: a fine diagonal ripstop, two threads crossing.
    vec3 q = p * 260.0;
    float a = sin(q.x + q.y) * sin(q.y - q.z + q.x * 0.3);
    float b = sin((q.x - q.y) * 0.5 + q.z);
    rmod = a * 0.04;
    return a * 0.55 + b * 0.25;
  }
  if (kind < 2.5) {
    // Armour: panel lines every few centimetres, cut into the plate, and a
    // faint orange-peel so a highlight breaks up the way paint does.
    float ang = atan(p.z, p.x);
    float gy = abs(fract(p.y * 14.0 + 0.5) - 0.5);
    float ga = abs(fract(ang * 1.9099 + 0.5) - 0.5);
    float groove = smoothstep(0.035, 0.0, gy) + smoothstep(0.03, 0.0, ga) * 0.8;
    float peel = hash3(floor(p * 900.0)) * 0.035;
    rmod = groove * 0.25;
    return -groove * 1.6 + peel;
  }
  // Brushed metal: streaks along one direction.
  float s = hash3(vec3(floor(p.y * 2400.0), floor(p.x * 30.0), 1.0));
  rmod = (s - 0.5) * 0.12;
  return s * 0.5;
}

void main() {
  vec3 n = normalize(vNormal);
  vec3 v = normalize(uEye - vWorld);
  float dist = length(uEye - vWorld);

  // Bump mapping from the relief, by screen-space derivatives: no tangents
  // to carry, and it fades by itself where the pattern is too fine to draw.
  float rmod;
  float h = relief(vLocal, uMat2.x, rmod);
  if (uMat2.x > 0.5) {
    vec3 dpdx = dFdx(vWorld), dpdy = dFdy(vWorld);
    float dhdx = dFdx(h), dhdy = dFdy(h);
    vec3 r1 = cross(dpdy, n), r2 = cross(n, dpdx);
    float det = dot(dpdx, r1);
    vec3 grad = sign(det) * (dhdx * r1 + dhdy * r2);
    float fade = clamp(1.6 - dist * 0.32, 0.0, 1.0);
    n = normalize(abs(det) * n - grad * 0.0016 * fade);
  }

  float rough = clamp(uMat.x + rmod, 0.045, 1.0);
  float metal = uMat.y;
  vec3 albedo = uColour;

  float ndl = max(dot(n, L), 0.0);
  float sh = shadowAt(vShadow, ndl);

  vec3 c = vec3(0.0);
  c += lightIt(n, v, L, vec3(1.00, 0.95, 0.88) * 3.1, albedo, rough, metal) * sh;
  c += lightIt(n, v, normalize(vec3(-0.62, 0.34, -0.60)), vec3(0.42, 0.60, 1.0) * 1.5,
               albedo, rough, metal);
  c += lightIt(n, v, normalize(vec3(0.75, 0.25, -0.35)), vec3(1.0, 0.62, 0.42) * 0.5,
               albedo, rough, metal);

  // Ambient: diffuse from the sky over the hemisphere, specular from the
  // sky in the mirror direction. The second is what makes metal metal.
  float ndv = max(dot(n, v), 1e-4);
  vec3 f0 = mix(vec3(0.04), albedo, metal);
  vec3 fres = f0 + (max(vec3(1.0 - rough), f0) - f0) * pow(1.0 - ndv, 5.0);
  vec3 amb = mix(vec3(0.020, 0.022, 0.028), vec3(0.055, 0.072, 0.115), n.y * 0.5 + 0.5);
  float occl = mix(0.72, 1.0, sh);
  c += albedo * amb * (1.0 - metal) * occl;
  c += envAt(reflect(-v, n), rough) * fres * occl * (1.0 - rough * 0.55);

  // What glows, glows regardless of the light.
  c += albedo * uMat.z;
  c = mix(c, c + vec3(0.25, 0.42, 0.60), uMat.w);

  c = mix(c, FOG, clamp((dist - 5.0) / 22.0, 0.0, 0.8));
  frag = vec4(tone(c), 1.0);
}`;

// The floor: dark and slightly wet, a measured grid on it that fades with
// distance, and the shadows of whoever is standing there.
const FLOOR_FRAG = `#version 300 es
${COMMON}
in vec3 vNormal;
in vec3 vWorld;
in vec3 vLocal;
in vec4 vShadow;
uniform vec3 uFocus;
out vec4 frag;
void main() {
  vec2 p = vWorld.xz;
  vec3 v = normalize(uEye - vWorld);
  vec3 n = vec3(0.0, 1.0, 0.0);
  float dist = length(uEye - vWorld);
  float d = length(p - uFocus.xz);
  float sh = shadowAt(vShadow, 1.0);

  vec2 f1 = abs(fract(p * 2.0 - 0.5) - 0.5) / max(fwidth(p * 2.0), vec2(1e-4));
  float l1 = 1.0 - min(min(f1.x, f1.y), 1.0);
  vec2 f2 = abs(fract(p * 0.5 - 0.5) - 0.5) / max(fwidth(p * 0.5), vec2(1e-4));
  float l2 = 1.0 - min(min(f2.x, f2.y), 1.0);
  float fade = exp(-d * 0.15);

  vec3 c = vec3(0.0085, 0.0105, 0.0165);
  // A polished floor: it shows the sky at a glancing angle and the key
  // light as a long highlight toward you.
  float fres = 0.04 + 0.96 * pow(1.0 - max(dot(n, v), 0.0), 5.0);
  c += envAt(reflect(-v, n), 0.22) * fres * 0.55 * mix(0.45, 1.0, sh);
  c += vec3(0.030, 0.050, 0.085) * exp(-d * d * 0.055);
  c *= mix(0.42, 1.0, sh);
  vec3 grid = vec3(0.10, 0.46, 0.85);
  c += grid * (l1 * 0.07 + l2 * 0.30) * fade;

  c = mix(c, FOG, clamp((dist - 5.0) / 22.0, 0.0, 0.92));
  frag = vec4(tone(c), 1.0);
}`;

const SKY_FRAG = `#version 300 es
${COMMON}
in vec3 vNormal;
in vec3 vWorld;
in vec3 vLocal;
in vec4 vShadow;
out vec4 frag;
void main() {
  vec3 d = normalize(vWorld - uEye);
  vec3 c = skyAt(d);
  vec3 q = floor(d * 420.0);
  float n = fract(sin(dot(q, vec3(12.9898, 78.233, 37.719))) * 43758.5453);
  float star = step(0.9982, n) * smoothstep(0.04, 0.45, d.y);
  c += vec3(0.75, 0.85, 1.0) * star * (0.30 + 0.70 * fract(n * 7.31)) * 0.8;
  frag = vec4(tone(c * 1.5), 1.0);
}`;

const DEPTH_VERT = `#version 300 es
layout(location=0) in vec3 pos;
uniform mat4 uLightVP, uModel;
void main() { gl_Position = uLightVP * uModel * vec4(pos, 1.0); }`;
const DEPTH_FRAG = `#version 300 es
precision highp float;
void main() {}`;

const TEX_VERT = `#version 300 es
layout(location=0) in vec3 pos;
uniform mat4 uViewProj, uModel;
out vec2 vUv;
void main() {
  vUv = vec2(pos.x + 0.5, 0.5 - pos.y);
  gl_Position = uViewProj * uModel * vec4(pos, 1.0);
}`;
const TEX_FRAG = `#version 300 es
precision highp float;
in vec2 vUv;
uniform sampler2D uTex;
uniform float uAlpha;
uniform float uGain;
out vec4 frag;
void main() {
  vec4 t = texture(uTex, vUv);
  if (t.a < 0.02) discard;
  frag = vec4(t.rgb * uGain, t.a * uAlpha);
}`;

// Light that spills: a soft additive disc, always facing you. Bloom, for
// the price of a quad.
const GLOW_VERT = `#version 300 es
layout(location=0) in vec3 pos;
uniform mat4 uProj, uView;
uniform vec3 uCenter;
uniform float uSize;
out vec2 vUv;
void main() {
  vUv = pos.xy * 2.0;
  vec4 c = uView * vec4(uCenter, 1.0);
  c.xy += pos.xy * uSize;
  gl_Position = uProj * c;
}`;
const GLOW_FRAG = `#version 300 es
precision highp float;
in vec2 vUv;
uniform vec3 uColour;
out vec4 frag;
void main() {
  float r = length(vUv);
  float a = exp(-r * r * 4.2) * (1.0 - smoothstep(0.75, 1.0, r));
  frag = vec4(uColour * a, 1.0);
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
    const uni = (prog, names) => {
      const o = { prog };
      for (const n of names) o[n] = gl.getUniformLocation(prog, n);
      return o;
    };
    const lit = ["uViewProj", "uModel", "uLightVP", "uEye", "uShadow", "uShadowOn"];
    this.pbr = uni(compile(gl, VERT, FRAG), [...lit, "uColour", "uMat", "uMat2"]);
    this.floor = uni(compile(gl, VERT, FLOOR_FRAG), [...lit, "uFocus"]);
    this.sky = uni(compile(gl, VERT, SKY_FRAG), lit);
    this.depth = uni(compile(gl, DEPTH_VERT, DEPTH_FRAG), ["uLightVP", "uModel"]);
    this.tex = uni(compile(gl, TEX_VERT, TEX_FRAG),
                   ["uViewProj", "uModel", "uTex", "uAlpha", "uGain"]);
    this.glow = uni(compile(gl, GLOW_VERT, GLOW_FRAG),
                    ["uProj", "uView", "uCenter", "uSize", "uColour"]);

    this.meshes = {};
    const parts = bodyMeshes();
    for (const name of Object.keys(parts)) this.meshes[name] = this.upload(parts[name]);

    // The shadow map: the scene's depth as the light sees it.
    this.shadowTex = gl.createTexture();
    gl.bindTexture(gl.TEXTURE_2D, this.shadowTex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.DEPTH_COMPONENT24, SHADOW_SIZE, SHADOW_SIZE, 0,
                  gl.DEPTH_COMPONENT, gl.UNSIGNED_INT, null);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_COMPARE_MODE, gl.COMPARE_REF_TO_TEXTURE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_COMPARE_FUNC, gl.LEQUAL);
    this.shadowFbo = gl.createFramebuffer();
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.shadowFbo);
    gl.framebufferTexture2D(gl.FRAMEBUFFER, gl.DEPTH_ATTACHMENT, gl.TEXTURE_2D,
                            this.shadowTex, 0);
    gl.drawBuffers([gl.NONE]);
    gl.readBuffer(gl.NONE);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);

    // One light, looking down at the middle of the room. Fixed: nothing in
    // this scene is far enough from the middle to need the map to follow.
    this.focus = [0, 0, -2.2];
    const centre = [0, 1.0, -2.2];
    const lightPos = V3.add(centre, V3.mul(LIGHT_DIR, 18));
    this.lightVP = M4.mul(M4.ortho(-8.5, 8.5, -8.5, 8.5, 1, 40),
                          M4.lookAt(lightPos, centre, [0, 1, 0]));

    gl.enable(gl.DEPTH_TEST);
    gl.depthFunc(gl.LEQUAL);
    gl.enable(gl.CULL_FACE);
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

  // A canvas, as a texture. Text is a thing a 2D canvas is excellent at and
  // a shader is not.
  texture(canvas, old) {
    const gl = this.gl;
    if (old) gl.deleteTexture(old);
    const tex = gl.createTexture();
    gl.activeTexture(gl.TEXTURE0);
    gl.bindTexture(gl.TEXTURE_2D, tex);
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, canvas);
    gl.generateMipmap(gl.TEXTURE_2D);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
    return tex;
  }

  // Pass one: what the light can see. Everything solid, once.
  shadowPass(list) {
    const gl = this.gl;
    gl.bindFramebuffer(gl.FRAMEBUFFER, this.shadowFbo);
    gl.viewport(0, 0, SHADOW_SIZE, SHADOW_SIZE);
    gl.clear(gl.DEPTH_BUFFER_BIT);
    gl.disable(gl.BLEND);
    gl.enable(gl.POLYGON_OFFSET_FILL);
    gl.polygonOffset(1.6, 3.0);
    gl.useProgram(this.depth.prog);
    gl.uniformMatrix4fv(this.depth.uLightVP, false, this.lightVP);
    for (const d of list) {
      if (d.noShadow) continue;
      const m = this.meshes[d.mesh];
      if (!m) continue;
      gl.uniformMatrix4fv(this.depth.uModel, false, d.model);
      gl.bindVertexArray(m.vao);
      gl.drawArrays(gl.TRIANGLES, 0, m.count);
    }
    gl.disable(gl.POLYGON_OFFSET_FILL);
    gl.bindVertexArray(null);
  }

  // Where the camera is, said once per view to every program that cares.
  camera(proj, view, eye, shadows) {
    const gl = this.gl;
    this.proj = proj;
    this.view = view;
    this.eye = eye;
    this.viewProj = M4.mul(proj, view);
    gl.activeTexture(gl.TEXTURE1);
    gl.bindTexture(gl.TEXTURE_2D, this.shadowTex);
    gl.activeTexture(gl.TEXTURE0);
    for (const p of [this.pbr, this.floor, this.sky]) {
      gl.useProgram(p.prog);
      gl.uniformMatrix4fv(p.uViewProj, false, this.viewProj);
      gl.uniformMatrix4fv(p.uLightVP, false, this.lightVP);
      gl.uniform3fv(p.uEye, eye);
      gl.uniform1i(p.uShadow, 1);
      gl.uniform1f(p.uShadowOn, shadows ? 1 : 0);
    }
  }

  drawSky() {
    const gl = this.gl;
    gl.useProgram(this.sky.prog);
    gl.depthMask(false);
    gl.cullFace(gl.FRONT);
    gl.uniformMatrix4fv(this.sky.uModel, false,
                        xf(T(this.eye[0], this.eye[1], this.eye[2]), S(80, 80, 80)));
    const m = this.meshes.sph;
    gl.bindVertexArray(m.vao);
    gl.drawArrays(gl.TRIANGLES, 0, m.count);
    gl.cullFace(gl.BACK);
    gl.depthMask(true);
  }

  drawFloor() {
    const gl = this.gl;
    gl.useProgram(this.floor.prog);
    gl.uniform3fv(this.floor.uFocus, this.focus);
    gl.uniformMatrix4fv(this.floor.uModel, false,
                        xf(T(0, 0, 0), RX(-Math.PI / 2), S(160, 160, 1)));
    const m = this.meshes.quad;
    gl.bindVertexArray(m.vao);
    gl.drawArrays(gl.TRIANGLES, 0, m.count);
  }

  drawOpaque(list) {
    const gl = this.gl;
    gl.disable(gl.BLEND);
    gl.useProgram(this.pbr.prog);
    let bound = null;
    for (const d of list) {
      const m = this.meshes[d.mesh];
      if (!m) continue;
      const mt = d.mat;
      gl.uniformMatrix4fv(this.pbr.uModel, false, d.model);
      gl.uniform3fv(this.pbr.uColour, mt.c);
      gl.uniform4f(this.pbr.uMat, mt.r, mt.m, mt.e || 0, d.sel || 0);
      gl.uniform4f(this.pbr.uMat2, mt.d || 0, 0, 0, 0);
      if (bound !== m) { gl.bindVertexArray(m.vao); bound = m; }
      gl.drawArrays(gl.TRIANGLES, 0, m.count);
    }
    gl.bindVertexArray(null);
  }

  // Things made of a texture: name plates, screens, holograms. After the
  // solid world, without writing depth, so a soft edge never cuts a hole.
  drawTextured(list) {
    const gl = this.gl;
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
    gl.depthMask(false);
    gl.disable(gl.CULL_FACE);
    gl.useProgram(this.tex.prog);
    gl.uniformMatrix4fv(this.tex.uViewProj, false, this.viewProj);
    gl.uniform1i(this.tex.uTex, 0);
    gl.activeTexture(gl.TEXTURE0);
    const m = this.meshes.quad;
    gl.bindVertexArray(m.vao);
    for (const d of list) {
      if (!d.tex) continue;
      gl.bindTexture(gl.TEXTURE_2D, d.tex);
      gl.uniformMatrix4fv(this.tex.uModel, false, d.model);
      gl.uniform1f(this.tex.uAlpha, d.alpha === undefined ? 1 : d.alpha);
      gl.uniform1f(this.tex.uGain, d.gain === undefined ? 1 : d.gain);
      gl.drawArrays(gl.TRIANGLES, 0, m.count);
    }
    gl.bindVertexArray(null);
    gl.enable(gl.CULL_FACE);
    gl.depthMask(true);
    gl.disable(gl.BLEND);
  }

  drawGlows(list) {
    const gl = this.gl;
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.ONE, gl.ONE);
    gl.depthMask(false);
    gl.useProgram(this.glow.prog);
    gl.uniformMatrix4fv(this.glow.uProj, false, this.proj);
    gl.uniformMatrix4fv(this.glow.uView, false, this.view);
    const m = this.meshes.quad;
    gl.bindVertexArray(m.vao);
    gl.disable(gl.CULL_FACE);
    for (const g of list) {
      gl.uniform3fv(this.glow.uCenter, g.at);
      gl.uniform1f(this.glow.uSize, g.size);
      gl.uniform3fv(this.glow.uColour, g.c);
      gl.drawArrays(gl.TRIANGLES, 0, m.count);
    }
    gl.enable(gl.CULL_FACE);
    gl.bindVertexArray(null);
    gl.depthMask(true);
    gl.disable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }
}

// ---------------------------------------------------------------- figures --
// A teammate: a jointed body in an armoured suit, posed every frame.
//
// Every visible thing about one says something, so a glance across the
// room is a status report:
//
//   the helmet         WHICH CLI. Each has its own silhouette -- a smooth
//                      dome, a square block with a slit, an open android
//                      face, a crest, twin lobes, wings -- because a head
//                      is what you recognise somebody by at a distance
//   the armour colour  THE ROLE, and what it carries says the role again:
//                      a reviewer has a lens, an architect a blueprint,
//                      an engineer a belt of tools, an archivist crystals
//   the lit strips     WHAT IT IS DOING: green working, amber waiting for
//                      review, blue at rest, dark when paused
//   the pose           the same, for anyone who is colour-blind or far
//                      away: a working agent is typing at a console, one
//                      waiting for review has its hand up
//
// Nothing is skinned. A suit of plates over joints is segmented by nature,
// so rigid parts on a hierarchy of matrices look right rather than cheap,
// and a figure costs fifty draw calls of geometry already on the card.

const mat = (c, r, m, e, d) => ({ c, r, m, e: e || 0, d: d || 0 });

function roleColour(role) {
  const r = (role || "").toLowerCase();
  if (/review|qa|test/.test(r)) return [.86, .58, .16];
  if (/architect|design|lead/.test(r)) return [.48, .36, .86];
  if (/engineer|dev|build/.test(r)) return [.16, .50, .82];
  if (/doc|writer/.test(r)) return [.20, .66, .46];
  if (/archiv|memory|librar/.test(r)) return [.70, .30, .52];
  if (/bridge|capab|ocr|voice/.test(r)) return [.88, .36, .56];
  const h = hashStr(r);
  const hue = (h % 360) / 360;
  const f = (n) => {
    const k = (n + hue * 12) % 12;
    return 0.52 - 0.30 * Math.max(-1, Math.min(Math.min(k - 3, 9 - k), 1));
  };
  return [f(0), f(8), f(4)];
}

// What it is doing, as a colour, a pose and how brightly it is lit.
function stateStyle(a) {
  const alive = a.status === "running" || a.status === "connected" || a.status === "waiting";
  if (a.paused) return { accent: [.42, .44, .50], pose: "off", emit: 0.25, word: "paused" };
  switch (a.task_state) {
    case "working":
      return { accent: [.22, .95, .55], pose: "work", emit: 2.6, word: "working" };
    case "addressing_feedback":
      return { accent: [1.0, .52, .18], pose: "work", emit: 2.6, word: "addressing feedback" };
    case "done_pending_review":
      return { accent: [1.0, .74, .20], pose: "wave", emit: 2.8, word: "awaiting review" };
    case "approved":
      return { accent: [.40, .72, 1.0], pose: "idle", emit: 2.0, word: "approved" };
    case "released":
      return { accent: [.40, .42, .48], pose: "off", emit: 0.3, word: "released" };
  }
  return alive
    ? { accent: [.30, .70, 1.0], pose: "idle", emit: 1.8, word: "idle" }
    : { accent: [.36, .37, .42], pose: "off", emit: 0.15, word: "no process" };
}

// Which head. The same six families the flat views draw as shapes.
function helmetFor(a) {
  const cli = (a.cli || "").toLowerCase();
  if (cli.startsWith("claude")) return "dome";
  if (cli.startsWith("codex")) return "block";
  if (cli.startsWith("opencode")) return "android";
  if (cli.startsWith("qwen")) return "crest";
  if (cli.startsWith("gemini")) return "twin";
  if (cli.startsWith("hermes")) return "wing";
  return "android";
}

// The crystal over the head: the shape the flat views use for this CLI.
function badgeFor(a) {
  const cli = (a.cli || "").toLowerCase();
  if (cli.startsWith("claude")) return "octa";
  if (cli.startsWith("codex")) return "cube";
  if (cli.startsWith("opencode")) return "sph";
  if (cli.startsWith("qwen")) return "tetra";
  if (cli.startsWith("gemini")) return "icosa";
  if (cli.startsWith("hermes")) return "cyl";
  return "icosa";
}

function isBridge(a) {
  return a.kind === "external" || a.provenance === "external" ||
         /bridge|capab/.test((a.role || "").toLowerCase());
}

function angleDiff(a, b) {
  let d = a - b;
  while (d > Math.PI) d -= Math.PI * 2;
  while (d < -Math.PI) d += Math.PI * 2;
  return d;
}

// One figure, as draw calls appended to `L`.
//
// o     the scene's record: where it stands, which way it faces
// t     seconds, for everything that moves
// eye   who is looking, so the head can turn to them
function drawFigure(L, o, t, eye, sel) {
  const a = o.data;
  if (isBridge(a)) { drawDrone(L, o, t, eye, sel); return; }
  const st = stateStyle(a);
  const seed = hashStr(a.name || "x");
  const rnd = (k) => ((seed >>> (k * 5)) & 31) / 31;
  const H = 0.955 + rnd(0) * 0.085;           // how tall
  const W = 0.95 + rnd(1) * 0.13;             // how broad
  const ph = rnd(2) * 6.28;

  const armour = roleColour(a.role || a.cli || "");
  const dim = st.pose === "off" ? 0.55 : 1;
  const SUIT = mat([.044 * dim, .049 * dim, .064 * dim], .92, 0, 0, 1);
  const PLATE = mat(armour.map((v) => v * dim), .40, .55, 0, 2);
  const TRIM = mat(armour.map((v) => (v * .30 + .035) * dim), .42, .75, 0, 3);
  const DARK = mat([.030, .033, .042], .55, .5, 0, 3);
  const GLOVE = mat([.040, .043, .054], .70, .1, 0, 1);
  const LIT = mat(st.accent, .4, 0, st.emit);
  const VISOR = mat([.012, .018, .030], .06, .92, 0);
  // Selection is said by the ring at their feet and the plate over their
  // head, not by washing the figure out: a highlight that hides the thing
  // it highlights is not one.
  const P = (mesh, model, m, noShadow) =>
    L.opaque.push({ mesh, model, mat: m, sel: sel ? 0.045 : 0, noShadow });

  // --- the pose --------------------------------------------------------
  const breath = Math.sin(t * 1.5 + ph) * 0.5 + 0.5;
  const sway = Math.sin(t * 0.55 + ph) * 0.022;
  const want = Math.atan2(eye[0] - o.pos[0], eye[2] - o.pos[2]);
  let headYaw = clamp(angleDiff(want, o.yaw), -1.05, 1.05);
  let headPitch = clamp(Math.atan2(eye[1] - (o.pos[1] + 1.68 * H),
                                   Math.hypot(eye[0] - o.pos[0], eye[2] - o.pos[2])) * -0.6,
                        -0.35, 0.35);
  let lean = 0.02 + breath * 0.012, twist = sway;
  // [left, right]
  let abd = [-0.10, 0.10], swing = [0.04, 0.04], elbow = [-0.22, -0.22],
      wrist = [0, 0], curl = [0.35, 0.35];
  if (st.pose === "idle") {
    swing = [0.05 + sway, 0.05 - sway];
    elbow = [-0.26 - breath * 0.03, -0.24 - breath * 0.03];
  } else if (st.pose === "work") {
    // Typing at a console that hangs in the air in front of the chest.
    const tap = (k) => Math.sin(t * (7.5 + k) + ph + k * 2.1) * 0.5 + 0.5;
    abd = [-0.16, 0.16];
    swing = [-0.42, -0.42];
    elbow = [-1.18 - tap(0) * 0.10, -1.18 - tap(1) * 0.10];
    wrist = [0.18 - tap(0) * 0.28, 0.18 - tap(1) * 0.28];
    curl = [0.55 + tap(2) * 0.3, 0.55 + tap(3) * 0.3];
    headYaw *= 0.25;
    headPitch = 0.30 + Math.sin(t * 0.8 + ph) * 0.04;
    lean = 0.07;
  } else if (st.pose === "wave") {
    // A hand up: this one is waiting for somebody.
    const w = Math.sin(t * 4.6 + ph);
    abd = [-0.10, 2.35 + w * 0.10];
    swing = [0.05, -0.15];
    elbow = [-0.24, -0.75 + w * 0.32];
    wrist = [0, w * 0.25];
    curl = [0.35, 0.05];
    twist = sway - 0.05;
  } else {
    // Nothing running: arms down, head bowed.
    headYaw = 0; headPitch = 0.42; lean = 0.05;
    elbow = [-0.10, -0.10];
  }

  // --- the skeleton ----------------------------------------------------
  const root = xf(T(o.pos[0], o.pos[1], o.pos[2]), RY(o.yaw), S(H, H, H));
  const pelvis = xf(root, T(0, 0.972 + breath * 0.003, 0));
  const wide = S(W, 1, 1);
  P("pelvis", xf(pelvis, wide), SUIT);
  P("belt", xf(pelvis, wide), TRIM);
  P("rbox", xf(pelvis, T(0, .072, .112), S(.052, .036, .020)), LIT, true);
  const abdomen = xf(pelvis, T(0, .10, 0), RX(lean));
  P("abdomen", xf(abdomen, wide), SUIT);
  const chest = xf(abdomen, T(0, .13, 0), RX(lean * 0.6), RY(twist), S(1, 1 + breath * 0.012, 1));
  P("chest", xf(chest, wide), SUIT);
  P("chestArmor", xf(chest, wide), PLATE);
  P("collar", chest, TRIM);
  // The strip down the sternum, and one across the back: lit by state.
  P("rbox", xf(chest, T(0, .185, .156 * 1), S(.030, .085, .012)), LIT, true);
  P("rbox", xf(chest, T(-.075 * W, .225, .146), S(.050, .012, .010)), LIT, true);
  P("rbox", xf(chest, T(.075 * W, .225, .146), S(.050, .012, .010)), LIT, true);
  // A pack on the back: where a suit keeps what powers it.
  P("rbox", xf(chest, T(0, .165, -.150), S(.250 * W, .270, .095)), TRIM);
  P("rbox", xf(chest, T(0, .165, -.202), S(.150 * W, .180, .020)), DARK);
  P("rbox", xf(chest, T(0, .235, -.214), S(.090, .014, .010)), LIT, true);

  const neck = xf(chest, T(0, .318, 0));
  P("neck", neck, SUIT);
  const head = xf(neck, T(0, .068, 0), RY(headYaw), RX(headPitch));
  drawHead(L, P, head, helmetFor(a), { PLATE, TRIM, DARK, LIT, VISOR, SUIT }, t, ph, st);

  const hands = [];
  for (let i = 0; i < 2; i++) {
    const side = i === 0 ? -1 : 1;
    const shoulder = xf(chest, T(side * .207 * W, .252, 0));
    P("sph", xf(shoulder, S(.118, .118, .118)), SUIT);
    P("pad", xf(shoulder, T(side * .016, .018, 0), RZ(-side * 0.42)), PLATE);
    P("rbox", xf(shoulder, T(side * .058, .012, 0), RZ(-side * 0.42), S(.012, .050, .070)),
      LIT, true);
    const upper = xf(shoulder, RZ(abd[i]), RX(swing[i]));
    P("upperArm", upper, SUIT);
    P("tube", xf(upper, T(0, -.115, 0), S(.114, .020, .118)), TRIM);
    const fore = xf(upper, T(0, -.292, 0), RX(elbow[i]));
    P("sph", xf(fore, S(.090, .090, .090)), TRIM);
    P("forearm", fore, SUIT);
    P("bracer", fore, PLATE);
    P("tube", xf(fore, T(0, -.075, 0), S(.118, .010, .120)), LIT, true);
    const hand = xf(fore, T(0, -.268, 0), RX(wrist[i]));
    P("palm", hand, GLOVE);
    P("fingers", xf(hand, T(0, -.100, 0), RX(-curl[i])), GLOVE);
    P("thumb", xf(hand, T(side * .036, -.040, .010), RZ(side * 0.55), RX(-0.3)), GLOVE);
    hands.push(hand);
  }

  for (const side of [-1, 1]) {
    const hip = xf(pelvis, T(side * .094 * W, -.032, 0), RZ(side * 0.045), RX(-0.07));
    P("thigh", hip, SUIT);
    P("thighPlate", hip, PLATE);
    const knee = xf(hip, T(0, -.435, 0), RX(0.13));
    P("kneeCap", knee, TRIM);
    P("shin", knee, SUIT);
    P("greave", knee, PLATE);
    P("rbox", xf(knee, T(0, -.20, .066), S(.014, .12, .008)), LIT, true);
    const ankle = xf(knee, T(0, -.415, 0), RX(-0.06), RZ(-side * 0.045));
    P("boot", ankle, TRIM);
    P("sole", ankle, DARK);
  }

  drawGear(L, P, a, { chest, pelvis, head, hands, root }, { PLATE, TRIM, DARK, LIT }, t, ph, st, W);

  // The console a working agent is typing at.
  if (st.pose === "work") {
    L.holo.push({
      kind: "console",
      model: xf(root, T(0, 1.075, .43), RX(-0.62), S(.56, .30, 1)),
      alpha: 0.78, gain: 1.5, t: t + ph,
    });
    L.glows.push({ at: M4.apply(root, [0, 1.08, .43]), size: .55,
                   c: st.accent.map((v) => v * 0.10) });
  }

  // The crystal over the head.
  const top = M4.apply(root, [0, 2.06 + Math.sin(t * 1.3 + ph) * .012, 0]);
  L.opaque.push({
    mesh: badgeFor(a),
    model: xf(T(top[0], top[1], top[2]), RY(t * (st.pose === "work" ? 1.4 : 0.5) + ph),
              RX(0.3), S(.105, .105, .105)),
    mat: mat(armour, .15, .1, 1.3), noShadow: true,
  });
  L.glows.push({ at: top, size: .34, c: armour.map((v) => v * 0.22) });

  // Chest name tape: the CLI it is, on the plate.
  L.decals.push({
    text: (a.cli || "agent").toUpperCase().slice(0, 12),
    model: xf(chest, T(0, .262, .1385), RX(-0.16), S(.105, .026, 1)),
  });
}

function drawHead(L, P, M, kind, m, t, ph, st) {
  const CERAMIC = mat([.72, .74, .78], .30, .15, 0);
  const blink = (Math.sin(t * 0.7 + ph * 3.0) > 0.985) ? 0.12 : 1;
  const eyes = (y, z, r) => {
    for (const s of [-1, 1]) {
      P("sph", xf(M, T(s * .036, y, z), S(r, r * blink, r * .6)), m.LIT, true);
    }
  };
  if (kind === "android") {
    // An open face: ceramic skull, a dark plate where a face goes, and two
    // eyes that blink.
    P("skull", M, CERAMIC);
    P("visor", xf(M, T(0, -.004, -.004), S(.985, 1.12, .985)), m.VISOR);
    eyes(.132, .112, .030);
    for (const s of [-1, 1]) {
      P("cyl", xf(M, T(s * .089, .128, -.004), RZ(Math.PI / 2), S(.062, .022, .062)), m.PLATE);
      P("cyl", xf(M, T(s * .100, .128, -.004), RZ(Math.PI / 2), S(.030, .012, .030)), m.LIT, true);
    }
    P("rbox", xf(M, T(0, .250, -.012), S(.022, .012, .13)), m.PLATE);
    P("jaw", xf(M, S(1.03, 1, 1.03)), m.TRIM);
    return;
  }
  // Helmets: a shell over the skull, and something to see out of.
  P("skull", M, m.SUIT);
  if (kind === "block") {
    P("rbox", xf(M, T(0, .128, -.004), S(.205, .262, .244)), m.PLATE);
    P("rbox", xf(M, T(0, .136, .112), S(.158, .030, .030)), m.LIT, true);
    P("rbox", xf(M, T(0, .060, .104), S(.110, .050, .040)), m.TRIM);
    for (const s of [-1, 1]) {
      P("rbox", xf(M, T(s * .108, .175, -.030), S(.020, .130, .034)), m.TRIM);
      P("rbox", xf(M, T(s * .108, .246, -.030), S(.012, .016, .020)), m.LIT, true);
    }
    return;
  }
  P("skull", xf(M, T(0, -.006, -.008), S(1.14, 1.075, 1.13)), m.PLATE);
  P("jaw", xf(M, S(1.10, 1, 1.08)), m.TRIM);
  P("visor", xf(M, S(1.02, 1, 1.03)), m.VISOR);
  // A lit line along the top of the visor: where the eyes would be.
  P("rbox", xf(M, T(0, .158, .118), S(.120, .008, .012)), m.LIT, true);
  if (kind === "dome") {
    P("rbox", xf(M, T(0, .268, -.030), S(.018, .034, .150)), m.TRIM);
  } else if (kind === "crest") {
    P("rbox", xf(M, T(0, .300, -.036), S(.014, .110, .200)), m.TRIM);
    P("rbox", xf(M, T(0, .304, -.036), S(.018, .090, .016)), m.LIT, true);
  } else if (kind === "twin") {
    for (const s of [-1, 1]) {
      P("sph", xf(M, T(s * .106, .150, -.006), S(.078, .078, .078)), m.TRIM);
      P("sph", xf(M, T(s * .122, .150, -.006), S(.040, .040, .040)), m.LIT, true);
    }
    P("torus", xf(M, T(0, .325, -.006), S(.21, .21, .21)), m.LIT, true);
  } else if (kind === "wing") {
    for (const s of [-1, 1]) {
      P("wing", xf(M, T(s * .098, .150, -.010), RZ(-s * 0.55), RY(s * 0.25)), m.TRIM);
      P("wing", xf(M, T(s * .104, .135, -.018), RZ(-s * 0.85), RY(s * 0.25), S(.8, .8, .8)),
        m.PLATE);
    }
  }
}

// What a role carries. The second way of saying what somebody is for.
function drawGear(L, P, a, b, m, t, ph, st, W) {
  const r = (a.role || "").toLowerCase();
  const HOLO = mat(m.LIT.c, .2, 0, 1.6);
  if (/review|qa|test/.test(r)) {
    // A lens over one eye, and a ring of checks turning at the shoulder.
    P("torus", xf(b.head, T(.036, .132, .124), RX(Math.PI / 2), S(.062, .062, .062)), m.PLATE);
    P("rbox", xf(b.head, T(.070, .132, .100), S(.012, .012, .050)), m.TRIM);
    const at = xf(b.chest, T(.34, .34 + Math.sin(t * 1.1 + ph) * .012, .06), RY(t * 0.9));
    P("torus", xf(at, RX(Math.PI / 2), S(.15, .15, .15)), HOLO, true);
    P("rbox", xf(at, T(-.018, -.010, 0), RZ(-0.75), S(.012, .050, .010)), HOLO, true);
    P("rbox", xf(at, T(.022, .008, 0), RZ(0.65), S(.012, .090, .010)), HOLO, true);
    L.glows.push({ at: M4.apply(at, [0, 0, 0]), size: .30, c: m.LIT.c.map((v) => v * 0.10) });
  } else if (/architect|design|lead/.test(r)) {
    // A blueprint: a frame within a frame, turning over the left hand.
    const at = xf(b.hands[0], T(0, -.20, .04), RY(t * 0.7 + ph), RX(0.5));
    const bar = (x, y, z, sx, sy, sz) => P("rbox", xf(at, T(x, y, z), S(sx, sy, sz)), HOLO, true);
    const e = .075, w = .008;
    for (const s1 of [-1, 1]) for (const s2 of [-1, 1]) {
      bar(s1 * e, s2 * e, 0, w, w, e * 2 + w);
      bar(s1 * e, 0, s2 * e, w, e * 2 + w, w);
      bar(0, s1 * e, s2 * e, e * 2 + w, w, w);
    }
    P("octa", xf(at, RY(-t * 1.6), S(.06, .06, .06)), mat(m.PLATE.c, .1, .2, 1.2), true);
    L.glows.push({ at: M4.apply(at, [0, 0, 0]), size: .36, c: m.LIT.c.map((v) => v * 0.10) });
    // And a long collar: the one who draws the plan dresses like it.
    P("tube", xf(b.chest, T(0, .335, -.008), S(.158, .060, .158)), m.TRIM);
  } else if (/engineer|dev|build/.test(r)) {
    // A belt of pouches and a device on the wrist.
    for (const [x, z] of [[-.135, .055], [.135, .055], [-.150, -.030], [.150, -.030]]) {
      P("rbox", xf(b.pelvis, T(x * W, .058, z), RY(Math.atan2(x, z)), S(.052, .070, .034)), m.TRIM);
    }
    P("rbox", xf(b.hands[0], T(0, .090, -.030), S(.058, .080, .022)), m.DARK);
    P("rbox", xf(b.hands[0], T(0, .090, -.043), S(.044, .060, .006)), HOLO, true);
  } else if (/doc|writer/.test(r)) {
    // Pages, hanging in the air where they can be read.
    for (let i = 0; i < 3; i++) {
      L.holo.push({
        kind: "page",
        model: xf(b.chest, T(-.36 - i * .035, .20 + i * .05 + Math.sin(t + i) * .008, .12 - i * .05),
                  RY(0.55), RX(-0.15), S(.15, .20, 1)),
        alpha: 0.62 - i * 0.12, gain: 1.4,
      });
    }
    P("rbox", xf(b.hands[1], T(0, -.10, .025), RX(0.6), S(.010, .120, .010)), m.PLATE);
  } else if (/archiv|memory|librar/.test(r)) {
    // Crystals racked on the pack: what the team knows, carried.
    for (let i = 0; i < 3; i++) {
      const c = xf(b.chest, T((i - 1) * .072, .355 + Math.sin(t * 1.2 + i) * .006, -.150),
                   RY(t * 0.8 + i), S(.045, .085, .045));
      P("octa", c, mat(m.LIT.c, .08, .1, 1.5), true);
      L.glows.push({ at: M4.apply(c, [0, 0, 0]), size: .14, c: m.LIT.c.map((v) => v * 0.12) });
    }
  }
}

// A teammate that is not a CLI: the capability bridge. It gets no body,
// because it has none -- a core in a pair of gimbal rings, hovering.
function drawDrone(L, o, t, eye, sel) {
  const a = o.data;
  const st = stateStyle(a);
  const tint = roleColour(a.role || "bridge");
  const ph = (hashStr(a.name || "b") % 628) / 100;
  const y = 1.28 + Math.sin(t * 1.1 + ph) * 0.045;
  const at = xf(T(o.pos[0], o.pos[1] + y, o.pos[2]), RY(o.yaw));
  const P = (mesh, model, m, ns) =>
    L.opaque.push({ mesh, model, mat: m, sel: sel ? 0.05 : 0, noShadow: ns });
  const SHELL = mat(tint, .28, .7, 0, 2);
  const DARK = mat([.03, .033, .042], .5, .6, 0, 3);
  const CORE = mat(st.accent, .1, 0, st.emit);
  P("sph", xf(at, S(.30, .30, .30)), DARK);
  P("sph", xf(at, S(.20, .20, .20)), CORE, true);
  for (const s of [-1, 1]) {
    P("sph", xf(at, T(0, s * .105, 0), S(.31, .13, .31)), SHELL);
  }
  // The lens it looks at you with.
  const face = Math.atan2(eye[0] - o.pos[0], eye[2] - o.pos[2]) - o.yaw;
  const lens = xf(at, RY(clamp(face, -1.2, 1.2)));
  P("cyl", xf(lens, T(0, 0, .135), RX(Math.PI / 2), S(.12, .05, .12)), DARK);
  P("sph", xf(lens, T(0, 0, .158), S(.085, .085, .03)), CORE, true);
  // Gimbals.
  P("torusFat", xf(at, RX(t * 0.9 + ph), S(.62, .62, .62)), SHELL);
  P("torus", xf(at, RZ(t * 0.6), RX(Math.PI / 2 + t * 0.4), S(.80, .80, .80)), CORE, true);
  P("rbox", xf(at, T(0, .22, 0), S(.012, .14, .012)), DARK);
  P("sph", xf(at, T(0, .30, 0), S(.028, .028, .028)), CORE, true);
  const c = M4.apply(at, [0, 0, 0]);
  L.glows.push({ at: c, size: .95, c: st.accent.map((v) => v * 0.16) });
  // The light it throws on the floor under it.
  L.glows.push({ at: [o.pos[0], o.pos[1] + .03, o.pos[2]], size: .9,
                 c: st.accent.map((v) => v * 0.07) });
}

// ------------------------------------------------------------------ scene --
// Where everything stands. Rebuilt when the snapshot changes; turned into
// draw calls every frame, because everything in it breathes.

function figureHeight(a) {
  const seed = hashStr(a.name || "x");
  return 0.955 + (((seed >>> 0) & 31) / 31) * 0.085;
}

class Scene {
  constructor(renderer) {
    this.r = renderer;
    this.nodes = [];
    this.graph = [];
    this.graphLinks = [];
    this.signs = [];
    this.sessions = [];
    this.cache = new Map();
    this.selected = null;
    this.hovered = null;
    this.mode = "team";
    this.graphCentre = [0, 1.45, -2.7];
  }

  // A canvas drawn once and kept as a texture, by key.
  canvasTex(key, w, h, paint) {
    let t = this.cache.get(key);
    if (!t) {
      const c = document.createElement("canvas");
      c.width = w; c.height = h;
      paint(c.getContext("2d"), w, h);
      t = { tex: this.r.texture(c), aspect: w / h };
      this.cache.set(key, t);
    }
    return t;
  }

  // The plate over a teammate: its name, and under it what it is and what
  // it is doing, with a bar in the colour of its state.
  plate(title, sub, accent) {
    const key = `plate|${title}|${sub}|${accent.join(",")}`;
    const font = `ui-monospace, "DejaVu Sans Mono", "Menlo", monospace`;
    const probe = document.createElement("canvas").getContext("2d");
    probe.font = `700 58px ${font}`;
    const w1 = probe.measureText(title).width;
    probe.font = `500 32px ${font}`;
    const w2 = sub ? probe.measureText(sub).width : 0;
    const w = Math.ceil(Math.max(w1, w2) + 84), h = sub ? 138 : 92;
    return this.canvasTex(key, w, h, (x) => {
      const r = 20;
      x.beginPath();
      x.moveTo(r, 0); x.arcTo(w, 0, w, h, r); x.arcTo(w, h, 0, h, r);
      x.arcTo(0, h, 0, 0, r); x.arcTo(0, 0, w, 0, r); x.closePath();
      x.fillStyle = "rgba(8,11,18,0.80)";
      x.fill();
      x.strokeStyle = "rgba(120,160,220,0.28)";
      x.lineWidth = 2;
      x.stroke();
      const rgb = accent.map((v) => Math.round(clamp(v, 0, 1) * 255)).join(",");
      x.fillStyle = `rgb(${rgb})`;
      x.fillRect(16, 16, 9, h - 32);
      x.textBaseline = "top";
      x.fillStyle = "#f2f6fc";
      x.font = `700 58px ${font}`;
      x.fillText(title, 44, 12);
      if (sub) {
        x.fillStyle = "#93a7c4";
        x.font = `500 32px ${font}`;
        x.fillText(sub, 46, 86);
      }
    });
  }

  decal(text) {
    return this.canvasTex("decal|" + text, 320, 80, (x, w, h) => {
      x.fillStyle = "rgba(4,6,10,0.72)";
      x.fillRect(0, 0, w, h);
      x.fillStyle = "#e6edf7";
      x.font = `700 46px ui-monospace, "DejaVu Sans Mono", monospace`;
      x.textAlign = "center";
      x.textBaseline = "middle";
      x.fillText(text, w / 2, h / 2 + 3);
    });
  }

  // What is on a working agent's console: code, in a few states of being
  // typed, so the screen changes while the hands move.
  consoleTex(step) {
    return this.canvasTex("console|" + step, 560, 300, (x, w, h) => {
      x.fillStyle = "rgba(6,22,34,0.55)";
      x.fillRect(0, 0, w, h);
      x.strokeStyle = "rgba(90,220,255,0.85)";
      x.lineWidth = 3;
      x.strokeRect(2, 2, w - 4, h - 4);
      const rows = [
        [30, 210], [56, 330], [56, 260], [82, 190], [82, 300], [56, 120],
        [30, 60], [30, 240], [56, 360], [56, 200 + step * 60],
      ];
      rows.forEach(([indent, len], i) => {
        x.fillStyle = i === rows.length - 1 ? "rgba(255,255,255,0.95)"
          : (i % 4 === 0 ? "rgba(120,255,190,0.85)" : "rgba(120,220,255,0.75)");
        x.fillRect(indent, 26 + i * 26, Math.min(len, w - indent - 30), 9);
      });
      x.fillStyle = "rgba(255,255,255,0.95)";
      x.fillRect(56 + 200 + step * 60 + 8, 26 + 9 * 26 - 4, 10, 17);
    });
  }

  pageTex() {
    return this.canvasTex("page", 300, 400, (x, w, h) => {
      x.fillStyle = "rgba(20,44,36,0.50)";
      x.fillRect(0, 0, w, h);
      x.strokeStyle = "rgba(140,255,200,0.8)";
      x.lineWidth = 3;
      x.strokeRect(2, 2, w - 4, h - 4);
      x.fillStyle = "rgba(200,255,225,0.9)";
      x.fillRect(28, 30, 150, 14);
      x.fillStyle = "rgba(160,240,205,0.65)";
      for (let i = 0; i < 11; i++) x.fillRect(28, 70 + i * 28, 240 - ((i * 53) % 90), 8);
    });
  }

  build(snap) {
    const all = ((snap && snap.detail && snap.detail.agents) || [])
      .filter((a) => a.cli !== "tmux");
    const every = [];
    for (const a of all) {
      const s = a.session || "";
      if (!every.includes(s)) every.push(s);
    }
    every.sort();
    // `only` is the dropdown the flat views have: one team on its own,
    // which on a fleet running four of them is the difference between a
    // crowd and a room you can read.
    const one = this.only && every.includes(this.only) ? this.only : null;
    const agents = one ? all.filter((a) => (a.session || "") === one) : all;
    const sessions = one ? [one] : every;
    const keep = this.selected && this.selected.data ? this.selected.data.name : null;
    this.nodes = [];
    this.signs = [];
    // One sector per session, side by side: two teams standing apart is
    // what two teams look like, and you are at the centre of the arc.
    const R = 2.7;
    const sector = Math.min(1.7, 2.4 / Math.max(sessions.length, 1));
    const first = -((sessions.length - 1) / 2) * sector;
    sessions.forEach((sname, si) => {
      const mine = agents.filter((a) => (a.session || "") === sname);
      const middle = first + si * sector;
      const spread = Math.min(sector * 0.80, 0.36 * Math.max(mine.length - 1, 1));
      mine.forEach((a, i) => {
        const k = mine.length === 1 ? 0 : (i / (mine.length - 1) - 0.5);
        const ang = middle + k * spread;
        const rr = R + (i % 2 ? 0.0 : 0.22);
        const x = Math.sin(ang) * rr, z = -Math.cos(ang) * rr;
        const drone = isBridge(a);
        const node = {
          kind: "agent", data: a,
          pos: [x, 0, z],
          yaw: Math.atan2(-x, -z),
          bounds: drone
            ? [{ c: [x, 1.28, z], r: 0.52 }]
            : [{ c: [x, 0.55, z], r: 0.50 }, { c: [x, 1.35, z], r: 0.45 }],
        };
        this.nodes.push(node);
        if (keep && a.name === keep) this.selected = node;
      });
      if (mine.length) {
        this.signs.push({
          text: "#" + (sname || "session"),
          sub: `${mine.length} seat${mine.length === 1 ? "" : "s"}`,
          // Out past the arc and low, where a sign on a floor would be.
          pos: [Math.sin(middle) * (R + 2.35), 0.30, -Math.cos(middle) * (R + 2.35)],
        });
      }
    });
    if (keep && !(this.selected && this.nodes.includes(this.selected))) this.selected = null;
    this.sessions = every;
    this.buildGraph(snap);
  }

  // The code graph: subsystems on a sphere around a point in front of you.
  // SIZE is how much is in one, SHAPE is what it is mostly made of, COLOUR
  // is where it lives in the tree.
  buildGraph(snap) {
    const g = ((snap && snap.detail && snap.detail.graph) || [])[0];
    const subs = ((g && g.subsystems) || []).slice(0, 40);
    this.graph = [];
    this.graphLinks = [];
    if (!subs.length) return;
    const n = subs.length;
    const golden = Math.PI * (3 - Math.sqrt(5));
    const C = this.graphCentre;
    const byId = new Map();
    subs.forEach((s, i) => {
      const y = 1 - (i / Math.max(n - 1, 1)) * 2;
      const r = Math.sqrt(Math.max(0, 1 - y * y));
      const th = golden * i + 0.6;
      const members = Array.isArray(s.members) ? s.members : [];
      const count = s.n || members.length || 1;
      const size = Math.min(.50, .15 + Math.log2(count + 1) * .058);
      const pos = [C[0] + Math.cos(th) * r * 1.55, C[1] + y * 0.92, C[2] + Math.sin(th) * r * 1.55];
      const node = {
        kind: "subsystem", data: s, pos, size,
        mesh: subsystemShape(members),
        colour: pathColour(s.path || s.name || ""),
        label: subsystemLabel(s, i),
        sub: `${count} symbol${count === 1 ? "" : "s"}` + (s.path ? ` · ${s.path}` : ""),
      };
      node.bounds = [{ c: pos, r: size * 0.75 }];
      this.graph.push(node);
      byId.set(s.id !== undefined ? s.id : s.name, node);
      if (s.name) byId.set(s.name, node);
    });
    const links = (g && g.links) || [];
    const heaviest = Math.max(1, ...links.map((l) => l.n || 1));
    for (const l of links) {
      const a = byId.get(l.a), b = byId.get(l.b);
      if (!a || !b || a === b) continue;
      this.graphLinks.push({ a, b, weight: (l.n || 1) / heaviest, certain: !!l.certain });
    }
  }

  active() { return this.mode === "graph" ? this.graph : this.nodes; }

  hit(origin, dir) {
    let best = null, bestT = Infinity;
    for (const o of this.active()) {
      for (const b of o.bounds || []) {
        const m = V3.sub(b.c, origin);
        const tca = V3.dot(m, dir);
        if (tca < 0) continue;
        const d2 = V3.dot(m, m) - tca * tca;
        if (d2 > b.r * b.r) continue;
        const tt = tca - Math.sqrt(b.r * b.r - d2);
        if (tt < bestT) { bestT = tt; best = o; }
      }
    }
    return best ? { object: best, t: bestT } : null;
  }

  // Everything, as lists of draw calls. `t` is seconds.
  collect(t, eye) {
    const L = { opaque: [], holo: [], glows: [], labels: [], decals: [] };
    this.decor(L, t);
    if (this.mode === "graph") this.collectGraph(L, t, eye);
    else this.collectTeam(L, t, eye);
    // Holograms and name tapes become textured quads.
    const tex = [];
    for (const h of L.holo) {
      const src = h.kind === "page" ? this.pageTex()
        : this.consoleTex(Math.floor((h.t || t) * 2.2) % 3);
      tex.push({ tex: src.tex, model: h.model, alpha: h.alpha, gain: h.gain });
    }
    for (const d of L.decals) {
      tex.push({ tex: this.decal(d.text).tex, model: d.model, alpha: 0.92, gain: 1.15 });
    }
    L.textured = tex.concat(L.labels);
    return L;
  }

  // The room itself: something for the eye to measure distance against.
  decor(L, t) {
    const STEEL = mat([.060, .068, .086], .32, .85, 0, 3);
    const n = 16;
    for (let i = 0; i < n; i++) {
      const a = (i / n) * Math.PI * 2 + 0.2;
      const h = 4.5 + ((i * 37) % 5) * 1.1;
      const x = Math.sin(a) * 15, z = -Math.cos(a) * 15 - 2;
      L.opaque.push({ mesh: "rbox", model: xf(T(x, h / 2, z), S(.42, h, .42)), mat: STEEL });
      const c = i % 3 === 0 ? [.25, .75, 1.0] : [.20, .45, .95];
      L.opaque.push({ mesh: "rbox", model: xf(T(x * 0.985, h * 0.5, z * 0.985 - 0.03), S(.05, h * 0.86, .05)),
                      mat: mat(c, .3, 0, 2.2), noShadow: true });
      L.glows.push({ at: [x * 0.985, h * 0.5, z * 0.985], size: 2.4, c: c.map((v) => v * 0.045) });
    }
    // A ring set into the floor where the team stands.
    L.opaque.push({ mesh: "torus", model: xf(T(0, .006, 0), S(5.4, 1, 5.4)),
                    mat: mat([.20, .55, 1.0], .3, 0, 1.4), noShadow: true });
    L.opaque.push({ mesh: "torus", model: xf(T(0, .006, 0), S(7.2, 1, 7.2)),
                    mat: mat([.14, .36, .80], .3, 0, 0.7), noShadow: true });
  }

  collectTeam(L, t, eye) {
    for (const o of this.nodes) {
      const a = o.data;
      const st = stateStyle(a);
      const sel = o === this.selected, hov = o === this.hovered;
      // What they stand on: a disc, with a ring lit by what they are doing.
      const [x, , z] = o.pos;
      L.opaque.push({ mesh: "cyl", model: xf(T(x, .013, z), S(.86, .026, .86)),
                      mat: mat([.045, .050, .064], .30, .85, 0, 3) });
      const pulse = sel ? 1.25 + Math.sin(t * 4) * 0.12 : (hov ? 1.1 : 1);
      L.opaque.push({ mesh: "torusFat", model: xf(T(x, .028, z), S(.80 * pulse, .28, .80 * pulse)),
                      mat: mat(st.accent, .3, 0,
                               (sel ? 1.9 : (hov ? 1.3 : 0.55)) * (st.emit > 1 ? 1 : 0.35)),
                      noShadow: true });
      L.glows.push({ at: [x, .05, z], size: sel ? 1.9 : 1.25,
                     c: st.accent.map((v) => v * (sel ? 0.13 : 0.05) * (st.emit > 1 ? 1 : 0.3)) });
      drawFigure(L, o, t, eye, sel || hov);

      const top = (isBridge(a) ? 1.95 : 2.36 * figureHeight(a)) +
                  ((hashStr(a.name || "") % 3) * 0.17);
      const bits = [a.role || a.cli || "", a.cli && a.role ? a.cli : "", st.word]
        .filter(Boolean).join(" · ");
      const p = this.plate(a.name || "agent", bits, st.accent);
      const hgt = 0.175, wid = hgt * p.aspect;
      const lp = [x, top + 0.02, z];
      L.labels.push({ tex: p.tex, model: xf(billboard(lp, eye), S(wid, hgt, 1)), alpha: 0.96 });
    }
    for (const s of this.signs) {
      const p = this.plate(s.text, s.sub, [.35, .60, .95]);
      const hgt = 0.24, wid = hgt * p.aspect;
      L.labels.push({ tex: p.tex, model: xf(billboard(s.pos, eye), S(wid, hgt, 1)), alpha: 0.75 });
    }
  }

  collectGraph(L, t, eye) {
    const C = this.graphCentre;
    // A table for the map to hang over.
    L.opaque.push({ mesh: "cyl", model: xf(T(C[0], .02, C[2]), S(4.2, .04, 4.2)),
                    mat: mat([.040, .046, .060], .28, .88, 0, 3) });
    L.opaque.push({ mesh: "torusFat", model: xf(T(C[0], .045, C[2]), S(4.2, .3, 4.2)),
                    mat: mat([.25, .65, 1.0], .3, 0, 1.6), noShadow: true });
    L.glows.push({ at: [C[0], .2, C[2]], size: 5.0, c: [.012, .030, .055] });

    for (const l of this.graphLinks) {
      const lit = this.selected === l.a || this.selected === l.b ||
                  this.hovered === l.a || this.hovered === l.b;
      const d = V3.sub(l.b.pos, l.a.pos);
      const len = V3.len(d);
      if (len < 1e-4) continue;
      const w = (l.certain ? 0.012 : 0.006) + l.weight * 0.022;
      const c = lit ? [.55, .88, 1.0] : (l.certain ? [.30, .45, .80] : [.30, .32, .46]);
      L.opaque.push({
        mesh: "tube",
        model: xf(alignY(l.a.pos, l.b.pos), T(0, len / 2, 0), S(w, len, w)),
        mat: mat(c, .3, .2, lit ? 2.4 : (l.certain ? 0.55 : 0.18)),
        noShadow: !lit,
      });
      if (lit) {
        // Something travelling along a lit edge: which way it depends.
        const k = (t * 0.6 + l.weight) % 1;
        const at = V3.lerp(l.a.pos, l.b.pos, k);
        L.glows.push({ at, size: .16, c: [.20, .34, .42] });
      }
    }
    for (const o of this.graph) {
      const sel = o === this.selected, hov = o === this.hovered;
      const s = o.size * (sel ? 1.14 : 1);
      const bob = Math.sin(t * 0.8 + o.pos[0] * 3) * 0.012;
      const at = [o.pos[0], o.pos[1] + bob, o.pos[2]];
      L.opaque.push({
        mesh: o.mesh,
        model: xf(T(at[0], at[1], at[2]), RY(t * 0.25 + o.pos[0]), RX(0.25), S(s, s, s)),
        mat: mat(o.colour, .16, .35, sel ? 0.9 : (hov ? 0.6 : 0.22), 0),
        sel: sel ? 0.25 : (hov ? 0.12 : 0),
      });
      L.glows.push({ at, size: s * (sel ? 2.6 : 1.9), c: o.colour.map((v) => v * (sel ? 0.16 : 0.07)) });
      if (sel || hov) {
        L.opaque.push({ mesh: "torus", model: xf(T(at[0], at[1], at[2]), RX(Math.PI / 2 + t * 0.7),
                                                  S(s * 1.7, s * 1.7, s * 1.7)),
                        mat: mat([.6, .9, 1.0], .3, 0, 2.2), noShadow: true });
      }
      const p = this.plate(o.label, (sel || hov) ? o.sub : "", o.colour.map((v) => Math.min(1, v * 1.3)));
      const hgt = (sel || hov) ? 0.17 : 0.105, wid = hgt * p.aspect;
      L.labels.push({ tex: p.tex,
                      model: xf(billboard([at[0], at[1] + s * 0.62 + hgt * 0.62, at[2]], eye),
                                S(wid, hgt, 1)),
                      alpha: 0.95 });
    }
  }
}

// A quad that faces the eye, turning only about Y so text never tilts.
function billboard(pos, eye) {
  const ang = Math.atan2(eye[0] - pos[0], eye[2] - pos[2]);
  return xf(T(pos[0], pos[1], pos[2]), RY(ang));
}

// What a subsystem is mostly MADE of, as a shape: functions behave, types
// define, a subsystem of files is a directory somebody grouped.
function subsystemShape(members) {
  const tally = {};
  for (const m of members) {
    const k = (m.kind || "").toLowerCase();
    tally[k] = (tally[k] || 0) + 1;
  }
  const top = Object.entries(tally).sort((a, b) => b[1] - a[1])[0];
  const kind = top ? top[0] : "";
  if (/fn|func|method/.test(kind)) return "sph";
  if (/struct|class|type|enum|interface/.test(kind)) return "rbox";
  if (/trait|protocol|impl/.test(kind)) return "octa";
  if (/file|module|mod/.test(kind)) return "cyl";
  if (/const|static|var/.test(kind)) return "tetra";
  return "icosa";
}

// Where it lives in the tree, as a colour: one crate, one hue.
function pathColour(path) {
  const parts = String(path).split("/").filter(Boolean);
  const key = parts.find((p) => !["src", "crates", "lib", "pkg", "app"].includes(p)) ||
              parts[0] || "";
  if (!key) return [.30, .50, .90];
  const hue = (hashStr(key) % 360) / 360;
  const f = (n) => {
    const k = (n + hue * 12) % 12;
    return 0.55 - 0.36 * Math.max(-1, Math.min(Math.min(k - 3, 9 - k), 1));
  };
  return [f(0), f(8), f(4)];
}

function subsystemLabel(s, i) {
  if (s.name) return s.name;
  if (s.path) {
    const parts = String(s.path).split("/").filter(Boolean);
    return parts.slice(-1)[0] || s.path;
  }
  const m = (Array.isArray(s.members) ? s.members : [])[0];
  if (m && m.name) return m.name.length > 20 ? m.name.slice(0, 19) + "…" : m.name;
  return `subsystem ${s.id !== undefined ? s.id : i}`;
}

// ----------------------------------------------------------------- panels --
// A screen in the room: a terminal, the chat, a menu of what the flat
// dashboard can do. One textured quad whose canvas is redrawn when its
// content changes; a ray against its plane gives the row under the
// pointer, which is how a line becomes a button.
class Panel {
  constructor(renderer, opts) {
    this.r = renderer;
    this.kind = opts.kind || "text";
    this.title = opts.title || "";
    this.lines = opts.lines || [];
    this.buttons = opts.buttons || [];
    this.pos = opts.pos || [0, 1.4, -1.6];
    this.yaw = opts.yaw || 0;
    this.w = opts.w || 1.1;
    this.cols = opts.cols || 54;
    this.rows = opts.rows || 18;
    this.accent = opts.accent || [.35, .65, 1.0];
    this.tex = null;
    this.dirty = true;
    this.row = -1;
    // A panel either stands in the room -- and so moves with the room when
    // you pull it past you -- or rides on your wrist and comes with you.
    // The menu is the second kind: a menu you have to go and find is a
    // menu nobody finds. `roomSpace` is set while it is actually mounted.
    this.wrist = !!opts.wrist;
    this.roomSpace = false;
    // A grid panel is a keyboard: rows of keys rather than one action per
    // row, so a ray has to resolve a column as well as a row.
    this.grid = opts.grid || null;
    this.col = -1;
    this.cursor = null;
    this.live = null;
    this.geom();
  }

  /// The texture is sized by the character grid; the panel in the room is
  /// sized in metres. Until a corner is dragged the second follows the
  /// first, which is what an opened screen should look like. After that the
  /// height is the operator's, and the grid follows the metres instead.
  geom() {
    this.cell = 30;
    this.cw = Math.round(this.cols * this.cell * 0.6 + 44);
    this.ch = this.rows * this.cell + 78;
    if (!this.freeH || !this.h) this.h = this.w * this.ch / this.cw;
    // Metres per character, remembered so that dragging a corner makes the
    // screen bigger rather than making the text bigger: a window resize,
    // not a zoom.
    this.cellM = this.w / Math.max(this.cols, 1);
    this.lineM = this.h / Math.max(this.rows, 1);
    this.rowTop = 62 / this.ch;
    this.rowH = this.cell / this.ch;
  }

  /// What is under a point on the panel's face, in the texture's own pixels
  /// so that a handle is the same size on a small panel as on a large one.
  ///
  /// The close button first; then an L of frame at each corner, which is
  /// exactly the bracket that is drawn there -- a handle you cannot see is a
  /// handle nobody finds, and one that is not where it is drawn is worse.
  /// Everything else is body, and the body moves the panel.
  zoneAt(u, v) {
    const px = u * this.cw, py = v * this.ch;
    if (!this.wrist && px > this.cw - 46 && py < 46) return "close";
    const band = 34, along = 80;
    const edge = px < band || px > this.cw - band || py < band || py > this.ch - band;
    const inL = px < along, inR = px > this.cw - along;
    const inT = py < along, inB = py > this.ch - along;
    if (edge && (inL || inR) && (inT || inB)) {
      return "corner:" + (inT ? "t" : "b") + (inL ? "l" : "r");
    }
    return "body";
  }

  /// A corner dragged to `w` by `h`: the grid is recomputed at the same
  /// character size, so a wider screen holds more columns.
  setSize(w, h) {
    this.w = clamp(w, 0.22, 7.0);
    this.h = clamp(h, 0.14, 5.0);
    this.freeH = true;
    const cols = Math.round(this.w / Math.max(this.cellM, 1e-4));
    const rows = Math.round(this.h / Math.max(this.lineM, 1e-4));
    const nc = clamp(cols, 20, 220), nr = clamp(rows, 4, 70);
    const changed = nc !== this.cols || nr !== this.rows;
    if (changed) {
      this.cols = nc;
      this.rows = nr;
      this.cell = 30;
      this.cw = Math.round(this.cols * this.cell * 0.6 + 44);
      this.ch = this.rows * this.cell + 78;
      this.rowTop = 62 / this.ch;
      this.rowH = this.cell / this.ch;
    }
    this.dirty = true;
    return changed;
  }

  set(lines) {
    const next = lines.slice(-this.rows);
    if (next.join("\n") !== this.lines.join("\n")) { this.lines = next; this.dirty = true; }
  }

  redraw() {
    const c = document.createElement("canvas");
    c.width = this.cw; c.height = this.ch;
    const x = c.getContext("2d");
    const rgb = this.accent.map((v) => Math.round(v * 255)).join(",");
    x.fillStyle = "rgba(6,9,15,0.90)";
    x.fillRect(0, 0, c.width, c.height);
    const g = x.createLinearGradient(0, 0, c.width, 0);
    g.addColorStop(0, `rgba(${rgb},0.55)`);
    g.addColorStop(1, `rgba(${rgb},0.10)`);
    x.fillStyle = g;
    x.fillRect(0, 0, c.width, 46);
    x.strokeStyle = this.focused ? "rgba(235,245,255,0.95)" : `rgba(${rgb},0.9)`;
    x.lineWidth = this.focused ? 7 : 4;
    x.strokeRect(2, 2, c.width - 4, c.height - 4);
    // The handles have to be visible to be grabbed: four corners you can
    // take hold of, and a close button that detaches.
    const hl = 54;
    const lit = this.zone && this.zone.startsWith("corner") ? this.zone.slice(7) : null;
    for (const [cx1, cy1, sx, sy, id] of [[9, 9, 1, 1, "tl"], [c.width - 9, 9, -1, 1, "tr"],
                                          [9, c.height - 9, 1, -1, "bl"],
                                          [c.width - 9, c.height - 9, -1, -1, "br"]]) {
      x.strokeStyle = lit === id ? "rgba(255,255,255,0.98)" : `rgba(${rgb},0.80)`;
      x.lineWidth = lit === id ? 11 : 8;
      x.beginPath();
      x.moveTo(cx1 + sx * hl, cy1);
      x.lineTo(cx1, cy1);
      x.lineTo(cx1, cy1 + sy * hl);
      x.stroke();
    }
    if (!this.wrist) {
      const on = this.zone === "close";
      x.fillStyle = on ? "rgba(255,120,120,0.85)" : "rgba(255,255,255,0.10)";
      x.fillRect(c.width - 44, 6, 38, 34);
      x.strokeStyle = on ? "#ffffff" : "#cfe0f5";
      x.lineWidth = 4;
      x.beginPath();
      x.moveTo(c.width - 35, 15); x.lineTo(c.width - 15, 33);
      x.moveTo(c.width - 15, 15); x.lineTo(c.width - 35, 33);
      x.stroke();
    }
    const font = `ui-monospace, "DejaVu Sans Mono", "Menlo", monospace`;
    x.textBaseline = "top";
    x.fillStyle = "#f4f8ff";
    x.font = `700 28px ${font}`;
    x.fillText(this.title, 20, 9);
    x.font = `500 ${this.cell - 7}px ${font}`;
    const chw = x.measureText("M").width || (this.cell * 0.6);
    if (this.grid) {
      const m = 22;        // clear of the frame, which is a resize handle
      this.grid.forEach((keys, i) => {
        const y = 62 + i * this.cell;
        const kw = (c.width - m * 2) / keys.length;
        keys.forEach((k, j) => {
          const on = i === this.row && j === this.col;
          const held = !!(k.held && k.held());
          x.fillStyle = on ? `rgba(${rgb},0.75)`
                           : held ? `rgba(${rgb},0.40)` : "rgba(255,255,255,0.06)";
          x.fillRect(m + j * kw + 2, y - 3, kw - 4, this.cell - 5);
          x.fillStyle = on ? "#ffffff" : "#d8e6f8";
          const label = String(k.label);
          const w = x.measureText(label).width;
          x.fillText(label, m + j * kw + Math.max(3, (kw - w) / 2), y);
        });
      });
      this.tex = this.r.texture(c, this.tex);
      this.dirty = false;
      return;
    }
    const rows = this.buttons.length ? this.buttons.map((b) => b.label) : this.lines;
    // Where the agent's own cursor is, drawn as the block a terminal draws.
    if (this.cursor && !this.buttons.length) {
      const [cx0, cy0] = this.cursor;
      if (cy0 >= 0 && cy0 < this.rows) {
        x.fillStyle = "rgba(220,235,255,0.55)";
        x.fillRect(22 + cx0 * chw, 62 + cy0 * this.cell - 2, Math.max(chw, 6), this.cell - 5);
      }
    }
    rows.forEach((line, i) => {
      const y = 62 + i * this.cell;
      const s = String(line);
      if (this.buttons.length) {
        const on = i === this.row;
        x.fillStyle = on ? `rgba(${rgb},0.60)` : "rgba(255,255,255,0.045)";
        x.fillRect(14, y - 3, c.width - 28, this.cell - 4);
        x.fillStyle = on ? "#ffffff" : "#cfe0f5";
      } else if (/^\s*[>$]/.test(s)) x.fillStyle = "#ffffff";
      else if (/^\s*\+/.test(s)) x.fillStyle = "#86f0b0";
      else if (/^\s*\[/.test(s)) x.fillStyle = "#ffcf70";
      else if (/→/.test(s) && !this.buttons.length) x.fillStyle = "#c4d6ee";
      else x.fillStyle = "#9db2cf";
      x.fillText(s.slice(0, this.cols), 22, y);
    });
    this.tex = this.r.texture(c, this.tex);
    this.dirty = false;
  }

  model() {
    return xf(T(this.pos[0], this.pos[1], this.pos[2]), RY(this.yaw), S(this.w, this.h, 1));
  }

  // Where a ray crosses this panel, in its own 0..1 coordinates.
  cross(origin, dir) {
    const rot = RY(this.yaw);
    const n = M4.dir(rot, [0, 0, 1]);
    const denom = V3.dot(n, dir);
    if (Math.abs(denom) < 1e-5) return null;
    const t = V3.dot(V3.sub(this.pos, origin), n) / denom;
    if (t < 0) return null;
    const local = V3.sub(V3.add(origin, V3.mul(dir, t)), this.pos);
    const u = V3.dot(local, M4.dir(rot, [1, 0, 0])) / this.w + 0.5;
    const v = 0.5 - local[1] / this.h;
    if (u < 0 || u > 1 || v < 0 || v > 1) return null;
    return { t, u, v };
  }
}

// ------------------------------------------------------------------ hands --
// Your own arms, drawn where the headset says your hands are.
//
// The headset reports a hand and a head, not an arm. The elbow is worked
// out: a shoulder a hand's width below and beside the head, two bones of
// known length, and the one place an elbow can be that satisfies both --
// pushed down and outward, which is where elbows go. It is the same
// sleeve and bracer the team wears, because you are in the room too.
function drawUserArm(K, grip, head, headM, handed, pinch, held, tint) {
  const side = handed === "left" ? -1 : 1;
  const SUIT = mat([.050, .056, .072], .82, 0, 0, 1);
  const PLATE = mat(tint, .34, .62, 0, 2);
  const GLOVE = mat([.040, .043, .054], .70, .1, 0, 1);
  const LIT = mat(held ? [1.0, .72, .25] : [.30, .75, 1.0], .4, 0, held ? 3.0 : 1.6);
  const P = (mesh, model, m) => K.push({ mesh, model, mat: m, noShadow: true });

  const hand = xf(grip, T(0, 0, .05), RX(Math.PI / 2));
  const wrist = M4.apply(hand, [0, 0, 0]);
  // Shoulders, from the head: level with the collarbone, to the side.
  let right = M4.dir(headM, [1, 0, 0]); right[1] = 0; right = V3.norm(right);
  let back = M4.dir(headM, [0, 0, 1]); back[1] = 0; back = V3.norm(back);
  const shoulder = V3.add(V3.add(V3.add(head, [0, -0.25, 0]), V3.mul(right, side * 0.20)),
                          V3.mul(back, 0.07));
  const L1 = 0.292, L2 = 0.268;
  let to = V3.sub(wrist, shoulder);
  let d = V3.len(to);
  const reach = L1 + L2 - 0.004;
  if (d > reach) { to = V3.mul(to, reach / d); d = reach; }
  const axis = V3.norm(to);
  const along = (L1 * L1 - L2 * L2 + d * d) / (2 * d);
  const off = Math.sqrt(Math.max(L1 * L1 - along * along, 0));
  let pole = V3.add([0, -1, 0], V3.mul(right, side * 0.7));
  pole = V3.norm(V3.sub(pole, V3.mul(axis, V3.dot(pole, axis))));
  const elbow = V3.add(V3.add(shoulder, V3.mul(axis, along)), V3.mul(pole, off));
  const end = d >= reach ? V3.add(shoulder, to) : wrist;

  // Limbs are authored hanging down -Y, so +Y points back up the bone.
  const upper = alignY(shoulder, V3.sub(shoulder, V3.sub(elbow, shoulder)));
  P("sph", xf(T(shoulder[0], shoulder[1], shoulder[2]), S(.118, .118, .118)), SUIT);
  P("upperArm", upper, SUIT);
  P("tube", xf(upper, T(0, -.115, 0), S(.114, .020, .118)), PLATE);
  const fore = alignY(elbow, V3.sub(elbow, V3.sub(end, elbow)));
  P("sph", xf(T(elbow[0], elbow[1], elbow[2]), S(.090, .090, .090)), PLATE);
  P("forearm", fore, SUIT);
  P("bracer", fore, PLATE);
  P("tube", xf(fore, T(0, -.075, 0), S(.118, .010, .120)), LIT);

  // The hand: a palm, four fingers of two joints each, and a thumb.
  const curl = clamp(pinch, 0, 1);
  P("palm", hand, GLOVE);
  [-.027, -.009, .009, .027].forEach((x, i) => {
    const len = [0.92, 1.05, 1.0, 0.82][side > 0 ? i : 3 - i];
    const a = xf(hand, T(x, -.100, 0), RX(-0.12 - curl * 1.05), S(1, len, 1));
    P("finger", a, GLOVE);
    P("finger", xf(a, T(0, -.046, 0), RX(-0.10 - curl * 1.25), S(.9, .86, .9)), GLOVE);
  });
  P("thumb", xf(hand, T(-side * .036, -.034, .008), RZ(-side * 0.75), RX(-0.35 - curl * 0.5)), GLOVE);
  P("rbox", xf(hand, T(0, -.045, -.021), S(.046, .050, .008)), LIT);
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
    headers, credentials: "same-origin", cache: "no-store",
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) throw new Error(`${path}: ${r.status}`);
  return r.json();
}

// A fleet to look at when there is no fleet to look at: the pictures in
// the README, and anybody opening this on a hub they have not paired with.
// Not real hostnames -- machine-a, machine-b, like every other example.
function demoSnapshot() {
  const mk = (name, cli, role, state, host, session) =>
    ({ name, cli, role, task_state: state, status: "running", host, session,
       provenance: "managed", paused: false });
  const sub = (name, path, n, kind) => ({
    name, path, n,
    members: Array.from({ length: Math.min(n, 6) }, (_, i) => ({ name: `${name}_${i}`, kind })),
  });
  return { detail: {
    agents: [
      mk("Ada", "claude", "architect", "working", "machine-a", "payments"),
      mk("Max", "claude", "engineer", "working", "machine-a", "payments"),
      mk("Iris", "opencode", "code reviewer", "done_pending_review", "machine-b", "payments"),
      mk("Rune", "codex", "engineer", "addressing_feedback", "machine-b", "payments"),
      { name: "CoderAI", cli: "bridge", role: "capability bridge", task_state: "idle",
        status: "running", host: "machine-c", session: "payments",
        provenance: "external", paused: false },
      mk("Quill", "qwen", "doc writer", "approved", "machine-b", "docs-site"),
      mk("Hermes", "hermes", "archivist", "idle", "machine-c", "docs-site"),
      mk("Gem", "gemini", "analyst", "idle", "machine-c", "docs-site"),
    ],
    graph: [{ subsystems: [
      sub("auth", "crates/hub/src/auth.rs", 34, "fn"),
      sub("wire", "crates/proto/src/lib.rs", 51, "struct"),
      sub("storage", "crates/hub/src/state.rs", 28, "fn"),
      sub("render", "crates/ctl/src/tui/draw.rs", 19, "fn"),
      sub("cli", "crates/ctl/src/main.rs", 42, "fn"),
      sub("node", "crates/node/src/spawn.rs", 37, "fn"),
      sub("hub", "crates/hub/src/api.rs", 66, "fn"),
      sub("memory", "crates/hub/src/memory.rs", 15, "struct"),
      sub("graph", "crates/hub/src/graph.rs", 23, "trait"),
      sub("voice", "crates/ctl/src/voice.rs", 12, "mod"),
      sub("drivers", "crates/node/src/driver.rs", 31, "trait"),
      sub("bridge", "crates/coderai/src/main.rs", 26, "fn"),
      sub("shim", "crates/mcpd/src/main.rs", 14, "const"),
      sub("schema", "crates/proto/schema.sql", 9, "file"),
    ], links: [
      { a: "hub", b: "wire", n: 41, certain: true }, { a: "hub", b: "storage", n: 33, certain: true },
      { a: "hub", b: "auth", n: 22, certain: true }, { a: "node", b: "wire", n: 38, certain: true },
      { a: "node", b: "cli", n: 17, certain: false }, { a: "cli", b: "render", n: 24, certain: true },
      { a: "cli", b: "voice", n: 12, certain: true }, { a: "hub", b: "memory", n: 15, certain: true },
      { a: "hub", b: "graph", n: 19, certain: true }, { a: "graph", b: "storage", n: 9, certain: false },
      { a: "auth", b: "storage", n: 7, certain: true }, { a: "render", b: "graph", n: 6, certain: false },
      { a: "node", b: "drivers", n: 29, certain: true }, { a: "drivers", b: "wire", n: 13, certain: true },
      { a: "bridge", b: "wire", n: 11, certain: true }, { a: "shim", b: "wire", n: 16, certain: true },
      { a: "shim", b: "node", n: 8, certain: false }, { a: "storage", b: "schema", n: 10, certain: true },
      { a: "bridge", b: "hub", n: 5, certain: false },
    ] }],
  } };
}

function demoTranscript(a) {
  const who = a.name || "agent";
  return [
    `$ ${a.cli || "claude"} --resume`,
    `[nexmesh] seat ${who} · ${a.role || "engineer"} · #${a.session || "payments"}`,
    "",
    "> add the refresh-token rotation to the login flow",
    "",
    "  reading src/auth.rs … 142 lines",
    "  reading src/store.rs … 88 lines",
    "  acquire_file src/auth.rs — held by nobody, taken",
    "",
    "  editing src/auth.rs",
    "  + pub fn rotate(refresh: &str) -> Result<Pair> {",
    "  +     let claims = verify(refresh)?;",
    "  +     if store::is_revoked(&claims.jti)? {",
    "  +         bail!(\"that refresh token has been used\");",
    "  +     }",
    "  +     store::revoke(&claims.jti)?;",
    "  +     mint(&claims.sub)",
    "  + }",
    "",
    "  cargo test -p auth … 14 passed",
    "  publish_work — pushed to main, the team has been told",
    "[PENDING MESSAGES] from Iris: looks right, one question about the jti index",
    "> ",
  ];
}

function demoChat() {
  return [
    "operator → (team): the login flow is the priority today",
    "Ada → (team): taking the token rotation, Max has the store",
    "Max → Ada: the store needs a revoked-jti index first",
    "Ada → Max: agreed, yours",
    "Rune → (team): addressing the review on the session cache",
    "Iris → (team): reviewing what Ada pushed",
    "Iris → Ada: looks right, one question about the jti index",
    "CoderAI → (team): transcribed 1 clip, 12s",
    "Quill → (team): docs updated for the new endpoint",
    "operator → Iris: approve it if the index is there",
  ];
}

// ------------------------------------------------------------ a live screen --
//
// A terminal in the room, attached rather than transcribed. It is the same
// websocket the dashboard and `nexmesh attach` use -- binary frames are
// bytes, text frames are control JSON -- and the same VT emulator the
// dashboard draws with (`term.js`), used headlessly: its DOM rendering is
// replaced, the screen it keeps is read out cell by cell, and the panel
// draws that as a texture. So a full-screen agent interface appears as the
// agent draws it, and keys typed in here reach the real process.
class LiveTerm {
  constructor(app, panel, agentId, name) {
    this.app = app;
    this.panel = panel;
    this.agent = agentId;
    this.name = name;
    this.readOnly = false;
    this.state = "attaching";
    this.needs = true;
    this.painted = 0;
    // term.js measures its own font against the DOM, so it needs a box with
    // a layout. Off-screen is enough; invisible is not the same as absent.
    this.host = document.createElement("div");
    this.host.style.cssText =
      "position:fixed;left:-20000px;top:0;width:1200px;height:600px;pointer-events:none";
    document.body.appendChild(this.host);
    this.t = new NexTerm(this.host);
    this.t.render = () => { this.needs = true; };   // drawn as a texture instead
    this.t.resize(panel.cols, panel.rows);
    const base = location.protocol === "https:" ? "wss://" : "ws://";
    const url = `${base}${location.host}/v1/attach/ws?agent=${encodeURIComponent(agentId)}`
              + `&cols=${panel.cols}&rows=${panel.rows}&replay=262144`;
    let ws = null;
    try {
      // A websocket cannot carry a header, so an operator token signs in
      // through the subprotocol. A dashboard account's cookie rides along.
      ws = token ? new WebSocket(url, ["nexmesh.token." + token]) : new WebSocket(url);
    } catch (e) {
      this.fail(String(e.message || e));
      return;
    }
    ws.binaryType = "arraybuffer";
    this.ws = ws;
    this.t.onInput((bytes) => { if (ws.readyState === 1) ws.send(bytes); });
    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        let v = {};
        try { v = JSON.parse(ev.data); } catch (e) { return; }
        if (v.t === "opened") {
          this.readOnly = !!v.read_only;
          this.t.readOnly = this.readOnly;
          this.state = this.readOnly ? "watching" : "attached";
          this.app.say(`${this.name}: ${this.state}`);
        } else if (v.t === "closed" || v.t === "error") {
          this.state = v.reason || v.message || "closed";
        }
        this.needs = true;
        return;
      }
      this.t.write(new Uint8Array(ev.data));
      this.needs = true;
    };
    ws.onclose = () => { this.state = "detached"; this.needs = true; };
    ws.onerror = () => { this.fail("could not attach"); };
  }

  /// An attach that cannot be had falls back to the transcript, which still
  /// says what the agent has been doing.
  fail(why) {
    this.state = why;
    this.needs = true;
    this.app.refreshTerminal().catch(() => {});
  }

  /// The screen, read out of the emulator and handed to the panel. Rate
  /// limited: a busy agent writes far more often than a frame is drawn, and
  /// nobody can read sixty screens a second.
  tick() {
    const now = performance.now();
    if (!this.needs || now - this.painted < 60) return;
    this.painted = now;
    this.needs = false;
    const t = this.t;
    const lines = [];
    for (let y = 0; y < t.rows; y++) {
      const row = t.screen.lines[y] || [];
      let line = "";
      for (let x = 0; x < t.cols; x++) line += (row[x] && row[x].c) || " ";
      lines.push(line.replace(/\s+$/, ""));
    }
    this.panel.lines = lines;
    this.panel.cursor = this.readOnly ? null : [t.cx, t.cy];
    this.panel.title = `${this.name} · ${this.state}`;
    this.panel.dirty = true;
  }

  /// A resized window is a resized terminal: the emulator's grid and the
  /// process's own idea of its screen both have to change, or a program
  /// draws boxes that do not close. The hub carries the same `resize` the
  /// dashboard sends, and it is rate limited because a drag is continuous
  /// and a full-screen program repaints on every one of them.
  resize(cols, rows) {
    if (cols === this.t.cols && rows === this.t.rows) return;
    this.t.resize(cols, rows);
    this.needs = true;
    const now = performance.now();
    this.want = [cols, rows];
    if (now - (this.told || 0) < 200) return;
    this.told = now;
    this.tell();
  }

  tell() {
    if (!this.want || !this.ws || this.ws.readyState !== 1) return;
    const [cols, rows] = this.want;
    try { this.ws.send(JSON.stringify({ t: "resize", cols, rows })); } catch (e) {}
  }

  /// What the keyboard in the room sends, and what a real keyboard sends
  /// when this screen has the focus.
  type(bytes) {
    if (this.readOnly) { this.app.say(`${this.name} is read-only`); return; }
    this.t.send(bytes);
  }

  close() {
    try { if (this.ws) { this.ws.onclose = null; this.ws.onerror = null; this.ws.close(); } }
    catch (e) {}
    try { this.host.remove(); } catch (e) {}
  }
}

class App {
  constructor() {
    this.canvas = document.getElementById("gl");
    this.r = new Renderer(this.canvas);
    this.scene = new Scene(this.r);
    this.snap = null;
    this.xr = null;
    this.refSpace = null;
    // The camera when there is no headset: it orbits a point, because a
    // picture of a room wants to move through it.
    this.cam = { yaw: 0, pitch: 0, dist: 0, focus: null };
    // Where the ROOM is, relative to you. Grabbing empty space and pulling
    // moves this: the world comes to you. Your arms are drawn outside it.
    this.world = { off: [0, 0, 0], yaw: 0 };
    this.grab = new Map();
    this.panels = [];
    this.pointers = [];
    // Which screen typing goes to, and the state of the keyboard in the
    // room. Both modifiers are one-shot.
    this.focus = null;
    this.kb = { shift: false, ctrl: false };
    this.status = "";
    this.fixed = null;            // a clock held still, for filming
    this.me = [.16, .62, .86];    // the colour of your own sleeves
    this.resize();
    window.addEventListener("resize", () => this.resize());
    this.wireDesktop();
    setInterval(() => {
      this.refreshTerminal().catch(() => {});
      this.refreshChat().catch(() => {});
    }, 1200);
  }

  now() { return this.fixed !== null ? this.fixed : performance.now(); }

  resize() {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    this.canvas.width = Math.max(2, Math.floor(this.canvas.clientWidth * dpr));
    this.canvas.height = Math.max(2, Math.floor(this.canvas.clientHeight * dpr));
  }

  async load() {
    if (DEMO) {
      this.snap = demoSnapshot();
      this.scene.build(this.snap);
      this.say("a demo fleet");
      return;
    }
    try {
      this.snap = await api("/v1/dashboard/state?session=*");
      this.scene.build(this.snap);
      const n = this.scene.nodes.length;
      if (!this.status || /agent/.test(this.status)) this.say(`${n} agent${n === 1 ? "" : "s"}`);
    } catch (e) {
      // A hub you have not signed in to is the ordinary case on a headset:
      // say so, and show something rather than an empty room.
      if (!this.snap) {
        this.snap = demoSnapshot();
        this.scene.build(this.snap);
      }
      this.say("not signed in: showing a demo fleet");
    }
  }

  /// Closing a screen has to close its attach as well: a websocket left
  /// open keeps a node streaming a terminal nobody is looking at.
  closeScreens(keep) {
    for (const q of this.panels) {
      if (keep && keep(q)) continue;
      if (q.live) { q.live.close(); q.live = null; }
      if (q === this.focus) this.focus = null;
    }
    this.panels = keep ? this.panels.filter(keep) : [];
  }

  say(text) {
    this.status = text;
    const el = document.getElementById("status");
    if (el) el.textContent = text;
  }

  worldMatrix() {
    return xf(T(this.world.off[0], this.world.off[1], this.world.off[2]), RY(this.world.yaw));
  }

  // ------------------------------------------------------------- screens --
  panel(kind, title, opts) {
    let p = this.panels.find((x) => x.kind === kind);
    if (!p) {
      p = new Panel(this.r, Object.assign({ kind, title }, opts || {}));
      this.panels.push(p);
    } else {
      p.title = title;
      // A screen the operator has sized by its corners keeps that size when
      // it is reused for another agent: having it snap back would undo the
      // one thing they did by hand.
      const kept = p.freeH ? { w: p.w, h: p.h, cols: p.cols, rows: p.rows } : null;
      Object.assign(p, opts || {});
      if (kept) Object.assign(p, kept);
      p.geom();
      p.dirty = true;
    }
    return p;
  }

  // A teammate's own screen, between you and them, facing you.
  //
  // It is an attach, not a transcript: the real screen, with the agent's
  // own cursor, and keys typed on the keyboard in the room reach the
  // process. Where there is no attach to be had -- an external participant,
  // an agent that has exited, the demo fleet -- it falls back to reading
  // the transcript, which still shows what happened.
  openTerminal(o) {
    const a = o.data;
    const st = stateStyle(a);
    const pos = [o.pos[0] * 0.52, 2.16, o.pos[2] * 0.52];
    const p = this.panel("terminal",
      `${a.name} · ${a.cli || ""}${a.host && a.host !== "-" ? " · " + a.host : ""}`, {
        pos, yaw: Math.atan2(-pos[0], -pos[2]),
        w: 1.55, cols: 60, rows: 17, accent: st.accent.map((v) => Math.min(1, v * 0.9 + 0.1)),
      });
    p.agent = a;
    p.opened = this.now();
    p.lines = [];
    p.cursor = null;
    if (p.live) { p.live.close(); p.live = null; }
    // The agent that HAS the terminal: for a seat in an adopted tmux pane
    // that is the pane's agent, not the seat's.
    const who = a.attach_agent_id || a.agent_id;
    if (!DEMO && who && typeof NexTerm === "function") {
      p.set([`attaching to ${a.name}…`]);
      p.live = new LiveTerm(this, p, who, a.name);
    } else {
      p.set([`connecting to ${a.name}…`]);
      this.refreshTerminal().catch(() => {});
    }
    this.focusPanel(p);
    // In a headset there is no other keyboard, so opening a screen puts one
    // in the room with it rather than leaving it a menu row away.
    if (this.xr) this.openKeys();
  }

  /// The close button on a screen: it detaches, which is the thing it
  /// looks like it does. The wrist menu has no close button, because
  /// closing the only way back would be a trap.
  closePanel(p) {
    if (!p || p.wrist) return;
    const name = p.live ? p.live.name : p.kind;
    if (p.live) { p.live.close(); p.live = null; }
    this.panels = this.panels.filter((q) => q !== p);
    if (this.focus === p) {
      this.focus = null;
      const next = this.panels.find((q) => q.kind === "terminal" && q.live);
      if (next) this.focusPanel(next);
    }
    this.say(p.kind === "terminal" ? `detached from ${name}` : `closed the ${p.kind}`);
  }

  /// A corner dragged: the panel is resized, and anything live inside it is
  /// told about it.
  sizePanel(p, w, h) {
    if (p.setSize(w, h) && p.live) p.live.resize(p.cols, p.rows);
  }

  /// Which screen the keyboard types into. With one open it is that one;
  /// with several, the one last opened or pressed.
  focusPanel(p) {
    for (const q of this.panels) {
      if (q.focused && q !== p) { q.focused = false; q.dirty = true; }
    }
    if (!p) { this.focus = null; return; }
    this.focus = p;
    p.focused = true;
    p.dirty = true;
    // On a flat screen a real keyboard is the obvious way to type, so the
    // emulator's own input is given the focus and term.js translates the
    // keys exactly as it does in the dashboard.
    if (!this.xr && p.live && p.live.t) p.live.t.focus();
    const k = this.panels.find((q) => q.kind === "keys");
    if (k) { k.title = `keyboard → ${p.agent ? p.agent.name : "a screen"}`; k.dirty = true; }
  }

  /// Send bytes to the focused screen.
  typeInto(bytes) {
    const p = this.focus && this.focus.live ? this.focus
            : this.panels.find((q) => q.kind === "terminal" && q.live);
    if (!p) { this.say("open a teammate's screen first"); return; }
    if (p !== this.focus) this.focusPanel(p);
    p.live.type(bytes);
  }

  async refreshTerminal() {
    const p = this.panels.find((x) => x.kind === "terminal");
    if (!p || !p.agent) return;
    // A live attach draws this panel itself.
    if (p.live && p.live.ws && p.live.ws.readyState <= 1 && p.live.state !== "could not attach") {
      return;
    }
    // The agent that HAS the terminal, which for a seat sitting in an
    // adopted tmux pane is the pane's agent and not the seat's. Reading the
    // seat's own transcript there returns nothing at all, which is a blank
    // screen with no explanation.
    const who = p.agent.attach_agent_id || p.agent.agent_id;
    if (DEMO || !who) {
      // Arriving a line at a time, the way a terminal does.
      const all = demoTranscript(p.agent);
      const n = clamp(5 + Math.floor((this.now() - p.opened) / 240), 5, all.length);
      p.set(all.slice(0, n));
      return;
    }
    try {
      const v = await api(
        `/v1/agents/${encodeURIComponent(who)}/transcript?tail=6000`);
      const text = (v.transcript || "")
        .replace(/\x1b\[[0-9;?]*[a-zA-Z]/g, "").replace(/\x1b[()][A-Z0-9]/g, "")
        .replace(/\r/g, "");
      const lines = text.split("\n").filter((l) => l.trim() !== "");
      // Nothing is not the same as a blank screen: a seat whose process has
      // printed nothing yet, or whose transcript lives elsewhere, should
      // say so rather than look broken.
      p.set(lines.length ? lines : [
        `${p.agent.name} has printed nothing yet.`,
        "",
        "A seat that was just started, or one whose CLI writes only to its",
        "own screen, has an empty transcript until it says something.",
      ]);
    } catch (e) {
      p.set([`could not read ${p.agent.name}'s screen: ${e.message || e}`]);
    }
  }

  openChat() {
    this.panel("chat", "the team", {
      pos: [-2.45, 2.00, -1.30], yaw: Math.atan2(2.45, 1.30),
      w: 1.35, cols: 52, rows: 12, accent: [.40, .85, .65],
    });
    this.refreshChat().catch(() => {});
  }

  async refreshChat() {
    const p = this.panels.find((x) => x.kind === "chat");
    if (!p) return;
    if (DEMO) { p.set(demoChat()); return; }
    const sel = this.scene.selected;
    const session = (sel && sel.data && sel.data.session) ||
      (this.scene.sessions && this.scene.sessions[0]) || "";
    if (!session) { p.set(["no session"]); return; }
    try {
      const v = await api(`/v1/sessions/${encodeURIComponent(session)}/log?limit=500`);
      p.set((v.messages || []).slice(-12)
        .map((m) => `${m.from} → ${m.to}: ${String(m.body).replace(/\n/g, " ")}`));
    } catch (e) { p.set(["could not read the log"]); }
  }


  // ------------------------------------------------------------ keyboard --
  //
  // A headset has no keyboard, and speech is not always the right way to
  // answer an agent: a path, a flag, a Ctrl-C. So there is a keyboard in
  // the room, pointed at with one hand and pressed with the trigger, and
  // what it sends goes to whichever screen has the focus.
  openKeys() {
    const p = this.panel("keys", "keyboard", {
      pos: [0, 1.12, -1.05], yaw: 0,
      w: 1.45, cols: 58, rows: 5, accent: [.50, .80, .70],
    });
    p.grid = this.keyGrid();
    p.geom();
    p.dirty = true;
    const f = this.focus && this.focus.kind === "terminal" ? this.focus
            : this.panels.find((q) => q.kind === "terminal");
    if (f) this.focusPanel(f);
    else p.title = "keyboard \u2192 no screen open";
    return p;
  }

  /// The layout. Rebuilt whenever a modifier changes, because a key whose
  /// label does not say what it will send is worse than no label.
  keyGrid() {
    const kb = this.kb;
    const SH = { "`": "~", "1": "!", "2": "@", "3": "#", "4": "$", "5": "%", "6": "^",
                 "7": "&", "8": "*", "9": "(", "0": ")", "-": "_", "=": "+",
                 "[": "{", "]": "}", "\\": "|", ";": ":", "'": "\"",
                 ",": "<", ".": ">", "/": "?" };
    const ch = (c) => ({
      label: kb.shift ? (SH[c] || c.toUpperCase()) : c,
      act: () => this.sendChar(c),
    });
    const key = (label, out) => ({ label, act: () => this.sendKey(out) });
    const mod = (label, name) => ({
      label, held: () => !!this.kb[name],
      act: () => {
        this.kb[name] = !this.kb[name];
        const k = this.panels.find((q) => q.kind === "keys");
        if (k) { k.grid = this.keyGrid(); k.dirty = true; }
      },
    });
    const row = (str) => str.split("").map(ch);
    return [
      [key("esc", "\u001b"), ...row("1234567890-="), key("bksp", "\u007f")],
      [key("tab", "\t"), ...row("qwertyuiop[]"), ch("\\")],
      [mod("ctrl", "ctrl"), ...row("asdfghjkl;'"), key("enter", "\r")],
      [mod("shift", "shift"), ...row("zxcvbnm,./"), key("\u2191", "\u001b[A"),
       key("\u2193", "\u001b[B")],
      [ch("`"), key("space", " "), key("\u2190", "\u001b[D"), key("\u2192", "\u001b[C"),
       key("pgup", "\u001b[5~"), key("pgdn", "\u001b[6~"),
       key("^C", "\u0003"), key("^D", "\u0004"), key("^L", "\u000c"),
       { label: "close", act: () => { this.panels = this.panels.filter((q) => q.kind !== "keys"); } }],
    ];
  }

  /// A printable key, with the modifiers applied. Both modifiers are
  /// one-shot, as they are on every on-screen keyboard: press shift, press
  /// a letter, and shift is spent.
  sendChar(c) {
    const kb = this.kb;
    const SH = { "`": "~", "1": "!", "2": "@", "3": "#", "4": "$", "5": "%", "6": "^",
                 "7": "&", "8": "*", "9": "(", "0": ")", "-": "_", "=": "+",
                 "[": "{", "]": "}", "\\": "|", ";": ":", "'": "\"",
                 ",": "<", ".": ">", "/": "?" };
    let out = kb.shift ? (SH[c] || c.toUpperCase()) : c;
    if (kb.ctrl) {
      const code = out.toLowerCase().charCodeAt(0);
      if (code >= 97 && code <= 122) out = String.fromCharCode(code - 96);
      else if (out === "[") out = "\u001b";
      else if (out === "\\") out = "\u001c";
      else if (out === "]") out = "\u001d";
    }
    this.sendKey(out);
  }

  sendKey(out) {
    this.typeInto(out);
    if (this.kb.shift || this.kb.ctrl) {
      this.kb.shift = false;
      this.kb.ctrl = false;
      const k = this.panels.find((q) => q.kind === "keys");
      if (k) { k.grid = this.keyGrid(); k.dirty = true; }
    }
  }

  // ---------------------------------------------------------- the tabs --
  //
  // Everything the flat dashboard keeps behind a tab, as a screen in the
  // room: the machines, the plan, what the team knows, and what is locked.
  // One panel, switched, because that is what a tab is.
  openView(which) {
    const d = (this.snap && this.snap.detail) || {};
    const pad = (v, n) => String(v == null ? "" : v).slice(0, n).padEnd(n);
    let title = which;
    let lines = [];
    if (which === "machines") {
      title = "the machines";
      for (const n of (this.snap && this.snap.nodes) || []) {
        lines.push(`${pad(n.name, 14)} ${pad(n.status, 11)} ${pad(n.arch, 14)} ${n.clis || ""}`);
      }
      if (!lines.length) lines.push("no machines are enrolled");
    } else if (which === "plan") {
      title = "the plan";
      const items = d.plan || [];
      const done = items.filter((i) => i.status === "done").length;
      lines.push(`${done} of ${items.length} done`);
      lines.push("");
      for (const i of items) {
        lines.push(`${pad(i.ord, 3)} ${pad(i.status, 13)} ${pad(i.label, 46)} ${i.who || ""}`);
      }
      if (!items.length) lines.push("no plan has been set for this team");
    } else if (which === "knowledge") {
      title = "what the team knows";
      for (const k of d.knowledge || []) {
        lines.push(`${pad(k.scope, 8)} ${pad(k.category, 10)} ${pad(k.key, 22)} ${k.value || ""}`);
      }
      if (!lines.length) lines.push("nothing is remembered yet");
    } else if (which === "files") {
      title = "files and who owns what";
      for (const l of d.locks || []) {
        lines.push(`locked ${pad(l.path, 40)} ${pad(l.who, 14)} ${l.purpose || ""}`);
      }
      for (const z of d.zones || []) {
        lines.push(`owns   ${pad(z.pattern, 40)} ${z.who || ""}`);
      }
      if (!lines.length) lines.push("nothing is locked, and no zones are set");
    }
    const p = this.panel("view", title, {
      pos: [2.30, 1.86, -1.05], yaw: Math.atan2(-2.30, 1.05),
      w: 1.60, cols: 78, rows: 20, accent: [.45, .60, .95],
    });
    p.buttons = [];
    p.grid = null;
    p.cursor = null;
    p.view = which;
    p.lines = lines.slice(0, p.rows);
    p.dirty = true;
    this.say(title);
    return p;
  }

  // Everything else the flat views can do, as a list you point at. In a
  // headset it rides on the left wrist, so there is always a way back to
  // the chat, to the code map, and to one team on its own.
  openMenu() {
    const sel = this.scene.selected;
    const who = sel && sel.data && sel.kind === "agent" ? sel.data.name : null;
    const here = (m) => (this.scene.mode === m ? "▸ " : "  ");
    const buttons = [
      { label: here("team") + "the team", act: () => this.setMode("team") },
      { label: here("graph") + "the code map", act: () => this.setMode("graph") },
      { label: "  the chat", act: () => this.openChat() },
      { label: "  " + (who ? who + "'s screen" : "a screen — point first"),
        act: () => (sel && sel.kind === "agent" ? this.openTerminal(sel)
                                                : this.say("point at somebody first")) },
      { label: "  the keyboard", act: () => this.openKeys() },
      { label: "  say something", act: () => this.listen() },
      { label: "  the machines", act: () => this.openView("machines") },
      { label: "  the plan", act: () => this.openView("plan") },
      { label: "  what the team knows", act: () => this.openView("knowledge") },
      { label: "  files and who owns what", act: () => this.openView("files") },
    ];
    // One line per team. This is the dropdown, in a place you can reach
    // without taking the headset off.
    const every = this.scene.sessions || [];
    if (every.length > 1) {
      buttons.push({ label: (this.scene.only ? "  " : "▸ ") + "every team",
                     act: () => this.showOnly(null) });
      for (const sname of every) {
        buttons.push({
          label: (this.scene.only === sname ? "▸ " : "  ") + "#" + (sname || "session"),
          act: () => this.showOnly(sname),
        });
      }
    }
    if (who) {
      buttons.push({ label: "  pause " + who, act: () => this.agentAction("pause") });
      buttons.push({ label: "  start " + who, act: () => this.agentAction("spawn") });
    }
    buttons.push({ label: "  close the screens",
                   act: () => this.closeScreens((x) => x.wrist) });
    const mounted = this.panels.find((x) => x.wrist && x.roomSpace);
    this.panel("menu", "nexmesh", {
      wrist: true,
      // Where it hangs when there is no wrist to hang it on: beside you,
      // as it always did on a flat screen.
      pos: mounted ? mounted.pos : [2.45, 2.00, -1.30],
      yaw: mounted ? mounted.yaw : Math.atan2(-2.45, 1.30),
      w: this.xr ? 0.30 : 1.05, cols: 28, rows: buttons.length,
      buttons, accent: [.55, .65, 1.0],
    });
  }

  // Show one team, or all of them. The ticks in the menu move with it.
  showOnly(sname) {
    this.scene.only = sname || null;
    if (this.snap) this.scene.build(this.snap);
    this.say(sname ? `#${sname} only` : "every team");
    this.openMenu();
  }

  async agentAction(what) {
    const sel = this.scene.selected;
    if (!sel || sel.kind !== "agent") { this.say("point at somebody first"); return; }
    if (DEMO || !sel.data.slot_id) { this.say(`${what}: ${sel.data.name}`); return; }
    try {
      await api(`/v1/slots/${encodeURIComponent(sel.data.slot_id)}/${what}`, {});
      this.say(`${what}: ${sel.data.name}`);
    } catch (e) { this.say(`could not ${what} that: ${e.message || e}`); }
  }

  // `origin`/`dir` is the ray in the room's own coordinates; `rOrigin`/
  // `rDir` is the same ray where your body is. A panel on your wrist does
  // not move when the room does, so it is tested against the second one.
  hitPanels(origin, dir, rOrigin, rDir) {
    let best = null;
    for (const p of this.panels) {
      const c = p.roomSpace ? p.cross(rOrigin || origin, rDir || dir)
                            : p.cross(origin, dir);
      if (c && (!best || c.t < best.c.t)) best = { p, c };
    }
    for (const p of this.panels) {
      const n = p.grid ? p.grid.length : p.buttons.length;
      const row = (best && best.p === p && n)
        ? Math.floor((best.c.v - p.rowTop) / p.rowH) : -1;
      const ok = row >= 0 && row < n ? row : -1;
      let col = -1;
      if (ok >= 0 && p.grid) {
        // The keys are drawn inside a margin, so the column is measured
        // against the same margin: a key you press has to be the key you
        // were pointing at, at both ends of the row.
        const keys = p.grid[ok] || [];
        const m = 22 / p.cw;
        const u = (best.c.u - m) / Math.max(1 - m * 2, 1e-3);
        col = Math.floor(u * keys.length);
        col = col >= 0 && col < keys.length ? col : -1;
      }
      const zone = best && best.p === p ? p.zoneAt(best.c.u, best.c.v) : null;
      if (p.row !== ok || p.col !== col || p.zone !== zone) {
        p.row = ok; p.col = col; p.zone = zone; p.dirty = true;
      }
    }
    if (!best) return null;
    // A corner or the close button is not a row: a key under your thumb at
    // the edge of the keyboard must not fire when you meant to take hold.
    const zone = best.p.zone || "body";
    if (zone !== "body") { best.p.row = -1; best.p.col = -1; }
    return { panel: best.p, row: best.p.row, col: best.p.col, zone, t: best.c.t };
  }

  panelsInto(L) {
    for (const p of this.panels) {
      if (p.live) p.live.tick();
      if (p.dirty || !p.tex) p.redraw();
      if (p.roomSpace) continue;        // drawn with your hands instead
      L.textured.push({ tex: p.tex, model: p.model(), alpha: 0.97, gain: 1.1 });
      const rgb = p.accent;
      L.glows.push({ at: p.pos, size: p.w * 1.5, c: rgb.map((v) => v * 0.035) });
    }
  }

  // --------------------------------------------------------------- input --
  wireDesktop() {
    const c = this.canvas;
    let down = false, lx = 0, ly = 0, sx = 0, sy = 0;
    c.addEventListener("pointerdown", (e) => {
      down = true; lx = sx = e.clientX; ly = sy = e.clientY;
      // A corner resizes the screen, the body moves it, and anywhere else
      // turns the room. The same three gestures the headset has.
      const h = this.lastRay && this.lastRay.panelHit;
      this.dragPanel = h ? h.panel : null;
      const zone = h ? (h.zone || "body") : null;
      this.dragHeld = zone && zone.startsWith("corner")
        ? { panel: this.dragPanel, corner: zone.slice(7), panelAt: this.dragPanel.pos.slice(),
            w0: this.dragPanel.w, h0: this.dragPanel.h, acc: [0, 0, 0] }
        : null;
      try { c.setPointerCapture(e.pointerId); } catch (err) {}
    });
    c.addEventListener("pointerup", (e) => {
      down = false;
      // The process is told the final size on release: a drag sends a few
      // on the way, and the last word has to be the size it ended at.
      if (this.dragHeld && this.dragHeld.panel.live) this.dragHeld.panel.live.tell();
      this.dragPanel = null;
      this.dragHeld = null;
      try { c.releasePointerCapture(e.pointerId); } catch (err) {}
      if (Math.abs(e.clientX - sx) < 4 && Math.abs(e.clientY - sy) < 4) this.press(this.lastRay);
    });
    c.addEventListener("pointermove", (e) => {
      if (down && this.dragHeld) {
        const d = this.worldDelta(this.dragHeld.panel, e.clientX - lx, e.clientY - ly);
        this.dragHeld.acc = V3.add(this.dragHeld.acc, d);
        this.dragCorner(this.dragHeld, this.dragHeld.acc);
        lx = e.clientX; ly = e.clientY;
      } else if (down && this.dragPanel) {
        this.movePanel(this.dragPanel, e.clientX - lx, e.clientY - ly);
        lx = e.clientX; ly = e.clientY;
      } else if (down) {
        this.cam.yaw -= (e.clientX - lx) * 0.005;
        this.cam.pitch = clamp(this.cam.pitch + (e.clientY - ly) * 0.004, -0.25, 1.2);
        lx = e.clientX; ly = e.clientY;
      }
      const rect = c.getBoundingClientRect();
      this.mouse = [((e.clientX - rect.left) / rect.width) * 2 - 1,
                    1 - ((e.clientY - rect.top) / rect.height) * 2];
    });
    c.addEventListener("wheel", (e) => {
      this.cam.dist = clamp(this.cam.dist + Math.sign(e.deltaY) * 0.3, -3.6, 6);
      e.preventDefault();
    }, { passive: false });
    window.addEventListener("keydown", (e) => {
      // While a screen has the focus, the keys are the agent's: term.js is
      // reading them from its own input, and `g` means g.
      if (this.typingNow()) return;
      if (e.key === "g") this.setMode(this.scene.mode === "graph" ? "team" : "graph");
      if (e.key === "v") this.listen();
      if (e.key === "c") this.openChat();
      if (e.key === "m") this.openMenu();
      if (e.key === "Escape") { this.closeScreens(); this.cam.focus = null; }
    });
  }

  // A screen pushed by so many pixels of mouse, which is a distance in the
  // room that depends on how far away the screen is: near ones follow the
  // hand, far ones drift, exactly as a thing at that distance should.
  movePanel(q, dx, dy) {
    const cam = this.desktopCamera();
    q.pos = V3.add(q.pos, this.worldDelta(q, dx, dy));
    q.yaw = Math.atan2(cam.eye[0] - q.pos[0], cam.eye[2] - q.pos[2]);
  }

  /// So many pixels of mouse, as a distance in the room at the depth the
  /// panel is at: near screens follow the hand, far ones drift, which is
  /// what things at those distances should do.
  worldDelta(q, dx, dy) {
    const cam = this.desktopCamera();
    const vd = V3.norm(V3.sub(q.pos, cam.eye));
    const right = V3.norm(V3.cross(vd, [0, 1, 0]));
    const up = V3.norm(V3.cross(right, vd));
    const depth = Math.max(0.5, V3.len(V3.sub(q.pos, cam.eye)));
    const k = 2 * Math.tan(cam.fov / 2) * depth / Math.max(this.canvas.clientHeight, 1);
    return V3.add(V3.mul(right, dx * k), V3.mul(up, -dy * k));
  }

  setMode(mode) {
    if (this.scene.mode === mode) return;
    this.scene.mode = mode;
    this.scene.selected = null;
    this.scene.hovered = null;
    this.closeScreens((p) => p.kind !== "terminal");
    this.r.focus = mode === "graph" ? [0, 0, this.scene.graphCentre[2]] : [0, 0, -2.2];
    this.say(mode === "graph" ? "the code graph" : "the team");
    const b = document.getElementById("modeBtn");
    if (b) b.textContent = mode === "graph" ? "the team" : "the code graph";
  }

  select(o, andOpen) {
    this.scene.selected = o;
    if (!o) return;
    if (o.kind === "agent") {
      const a = o.data;
      this.say(`${a.name}: ${a.role || a.cli}, ${stateStyle(a).word}` +
               (a.host && a.host !== "-" ? `, on ${a.host}` : ""));
      // Clicking a teammate opens their screen: that is what clicking a
      // person in this room means.
      if (andOpen !== false) this.openTerminal(o);
    } else {
      this.say(`${o.label}: ${o.sub}`);
    }
  }

  // A press, wherever the ray is: a button, a teammate, a subsystem, or
  // nothing at all, which starts listening.
  press(ray) {
    if (ray && ray.panelHit) {
      const h = ray.panelHit, p = h.panel;
      if (h.zone === "close") { this.closePanel(p); return; }
      if (h.zone && h.zone.startsWith("corner")) return;   // that is a handle
      if (p.grid && h.row >= 0 && h.col >= 0) {
        const k = (p.grid[h.row] || [])[h.col];
        if (k && k.act) { k.act(); return; }
      }
      if (p.buttons.length && h.row >= 0) {
        const b = p.buttons[h.row];
        if (b && b.act) { b.act(); return; }
      }
      // Pressing a screen is how you choose which one the keyboard types
      // into, when more than one is open.
      if (p.kind === "terminal") this.focusPanel(p);
      return;
    }
    if (ray && ray.hit) { this.select(ray.hit.object); return; }
    if (!this.xr) { this.blurTerminal(); return; }
    this.listen();
  }

  /// Is a real keyboard currently typing into an agent?
  typingNow() {
    const a = document.activeElement;
    return !!(a && a.classList && a.classList.contains("ntinput"));
  }

  /// Give the keys back to the page.
  blurTerminal() {
    if (!this.typingNow()) return;
    try { document.activeElement.blur(); } catch (e) {}
    if (this.focus) { this.focus.focused = false; this.focus.dirty = true; }
  }

  desktopCamera() {
    const graph = this.scene.mode === "graph";
    const at = this.cam.focus || (graph ? this.scene.graphCentre : [0, 1.32, -2.40]);
    const d = Math.max(0.6, (this.cam.focus ? 3.1 : (graph ? 5.0 : 6.0)) + this.cam.dist);
    const el = 0.105 + this.cam.pitch;
    const eye = [at[0] + Math.sin(this.cam.yaw) * Math.cos(el) * d,
                 at[1] + Math.sin(el) * d,
                 at[2] + Math.cos(this.cam.yaw) * Math.cos(el) * d];
    const fov = 0.74;
    const aspect = this.canvas.width / Math.max(this.canvas.height, 1);
    return { eye, fov, aspect, view: M4.lookAt(eye, at, [0, 1, 0]),
             proj: M4.perspective(fov, aspect, 0.05, 200) };
  }

  // ------------------------------------------------------------- a frame --
  frame(now, xrFrame) {
    const gl = this.r.gl;
    const t = now / 1000;
    if (xrFrame) { this.frameXR(t, xrFrame); return; }
    const cam = this.desktopCamera();
    if (this.mouse) {
      const inv = M4.invert(cam.view);
      const tan = Math.tan(cam.fov / 2);
      const dir = V3.norm(M4.dir(inv, [this.mouse[0] * tan * cam.aspect, this.mouse[1] * tan, -1]));
      const onPanel = this.hitPanels(cam.eye, dir);
      const hit = onPanel ? null : this.scene.hit(cam.eye, dir);
      this.lastRay = { hit, panelHit: onPanel };
      this.scene.hovered = hit ? hit.object : null;
      this.canvas.style.cursor = (hit || (onPanel && onPanel.row >= 0)) ? "pointer" : "grab";
    }
    const L = this.scene.collect(t, cam.eye);
    this.panelsInto(L);
    this.r.shadowPass(L.opaque);
    gl.bindFramebuffer(gl.FRAMEBUFFER, null);
    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    gl.depthMask(true);
    gl.clearColor(0.02, 0.03, 0.05, 1);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    this.r.camera(cam.proj, cam.view, cam.eye, true);
    this.r.drawSky();
    this.r.drawFloor();
    this.r.drawOpaque(L.opaque);
    this.r.drawTextured(L.textured);
    this.r.drawGlows(L.glows);
  }

  frameXR(t, xrFrame) {
    const gl = this.r.gl;
    const pose = xrFrame.getViewerPose(this.refSpace);
    if (!pose) return;
    const layer = this.xr.renderState.baseLayer;
    const headM = pose.transform.matrix;
    const head = [headM[12], headM[13], headM[14]];
    this.mountWrist(head);
    this.updatePointers(xrFrame);
    this.applyGrab();
    this.applyStick();
    const world = this.worldMatrix();
    const inv = M4.invert(world);
    this.headAt = M4.apply(inv, head);
    const L = this.scene.collect(t, M4.apply(inv, head));
    this.panelsInto(L);
    // Everything that is yours rather than the room's: arms, rays, and
    // the plate on your wrist that says what just happened.
    const K = [], KT = [];
    for (const p of this.pointers) {
      if (p.grip) {
        drawUserArm(K, p.grip, head, headM, p.handed, p.pinch, this.grab.has(p.src), this.me);
      }
      const len = p.far || 5;
      K.push({ mesh: "tube",
               model: xf(alignY(p.origin, V3.add(p.origin, p.dir)), T(0, len / 2, 0),
                         S(.005, len, .005)),
               mat: mat(p.on ? [.55, .90, 1.0] : [.30, .42, .60], .3, 0, p.on ? 2.6 : 0.9),
               noShadow: true });
    }
    const left = this.pointers.find((p) => p.handed === "left" && p.grip);
    const plate = this.scene.plate(this.status || "nexmesh", "", [.30, .70, 1.0]);
    const at = left ? V3.add([left.grip[12], left.grip[13], left.grip[14]], [0, .12, 0])
                    : V3.add(head, V3.add(V3.mul(M4.dir(headM, [0, 0, -1]), 0.9), [0, -.38, 0]));
    const hh = left ? 0.045 : 0.07;
    KT.push({ tex: plate.tex, model: xf(billboard(at, head), S(hh * plate.aspect, hh, 1)),
              alpha: 0.95 });
    // Whatever is strapped to you is drawn where you are, so pulling the
    // room past you leaves it exactly where your hand is.
    for (const p of this.panels) {
      if (p.roomSpace) KT.push({ tex: p.tex, model: p.model(), alpha: 0.98, gain: 1.15 });
    }

    this.r.shadowPass(L.opaque);
    gl.bindFramebuffer(gl.FRAMEBUFFER, layer.framebuffer);
    gl.depthMask(true);
    gl.clearColor(0.02, 0.03, 0.05, 1);
    gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
    for (const view of pose.views) {
      const vp = layer.getViewport(view);
      gl.viewport(vp.x, vp.y, vp.width, vp.height);
      const vpos = [view.transform.position.x, view.transform.position.y,
                    view.transform.position.z];
      const raw = view.transform.inverse.matrix;
      // The room is drawn THROUGH the world transform; your arms are not.
      this.r.camera(view.projectionMatrix, M4.mul(raw, world), M4.apply(inv, vpos), true);
      this.r.drawSky();
      this.r.drawFloor();
      this.r.drawOpaque(L.opaque);
      this.r.drawTextured(L.textured);
      this.r.drawGlows(L.glows);
      this.r.camera(view.projectionMatrix, raw, vpos, false);
      this.r.drawOpaque(K);
      this.r.drawTextured(KT);
    }
  }

  // The menu rides a hand's breadth above the left controller, turned to
  // face you, so turning your wrist up is how you read it. It uses the
  // hand as of the last frame, which is a sixtieth of a second behind and
  // looks like nothing at all.
  mountWrist(head) {
    const m = this.panels.find((p) => p.wrist);
    if (!m) return;
    const left = (this.pointers || []).find((p) => p.handed === "left" && p.grip);
    if (!left) {
      if (m.roomSpace) { m.roomSpace = false; m.w = 1.05; m.geom(); m.dirty = true; }
      return;
    }
    if (!m.roomSpace) { m.roomSpace = true; m.w = 0.30; m.geom(); m.dirty = true; }
    const g = left.grip;
    const at = [g[12], g[13] + 0.17 + m.h / 2, g[14]];
    m.pos = at;
    m.yaw = Math.atan2(head[0] - at[0], head[2] - at[2]);
  }

  // Controllers and hands: where they are, what they point at.
  updatePointers(xrFrame) {
    this.pointers = [];
    const inv = M4.invert(this.worldMatrix());
    let hovered = null;
    for (const src of this.xr.inputSources) {
      if (!src.targetRaySpace) continue;
      const pose = xrFrame.getPose(src.targetRaySpace, this.refSpace);
      if (!pose) continue;
      const m = pose.transform.matrix;
      const origin = [m[12], m[13], m[14]];
      const dir = V3.norm(M4.dir(m, [0, 0, -1]));
      const sOrigin = M4.apply(inv, origin);
      const sDir = V3.norm(M4.dir(inv, dir));
      const onPanel = this.hitPanels(sOrigin, sDir, origin, dir);
      const hit = onPanel ? null : this.scene.hit(sOrigin, sDir);
      const gripPose = src.gripSpace ? xrFrame.getPose(src.gripSpace, this.refSpace) : null;
      const g = src.gamepad;
      const squeeze = g && g.buttons[1] ? g.buttons[1].value : 0;
      const trigger = g && g.buttons[0] ? g.buttons[0].value : 0;
      this.pointers.push({
        src, origin, dir, sOrigin, sDir, hit, panelHit: onPanel,
        on: !!(hit || (onPanel && onPanel.row >= 0)),
        far: onPanel ? onPanel.t : (hit ? hit.t : 5),
        grip: gripPose ? gripPose.transform.matrix : m,
        pinch: Math.max(squeeze, trigger), squeeze,
        handed: src.handedness,
      });
      if (hit) hovered = hit.object;
    }
    this.scene.hovered = hovered;
  }

  // Taking hold. Squeezing on a screen moves that screen, squeezing on a
  // subsystem takes that one, and squeezing on empty space grabs the ROOM
  // and pulls it past you -- so both the map and the screens around it can
  // be put into the shape in your head. The trigger still presses buttons,
  // so holding a thing and using it are different hands' work.
  applyGrab() {
    for (const p of this.pointers) {
      const held = this.grab.get(p.src);
      const squeezing = p.squeeze > 0.6;
      if (squeezing && !held) {
        const panel = p.panelHit && !p.panelHit.panel.roomSpace ? p.panelHit.panel : null;
        const node = !panel && this.scene.mode === "graph" && p.hit ? p.hit.object : null;
        // A corner is a handle: squeezing one resizes the screen, squeezing
        // anywhere else on it moves the screen.
        const zone = panel ? (p.panelHit.zone || "body") : null;
        const corner = zone && zone.startsWith("corner") ? zone.slice(7) : null;
        this.grab.set(p.src, {
          at: p.sOrigin, node, panel, corner,
          nodeAt: node ? node.pos.slice() : null,
          panelAt: panel ? panel.pos.slice() : null,
          w0: panel ? panel.w : 0, h0: panel ? panel.h : 0,
        });
      } else if (!squeezing && held) {
        if (held.panel && held.corner && held.panel.live) held.panel.live.tell();
        this.grab.delete(p.src);
      } else if (squeezing && held) {
        const d = V3.sub(p.sOrigin, held.at);
        if (held.panel && held.corner) {
          // The corner follows the hand and the opposite corner stays put,
          // which is how a window behaves and the only way a resize feels
          // like one.
          this.dragCorner(held, d);
        } else if (held.panel) {
          const q = held.panel;
          q.pos = V3.add(held.panelAt, d);
          // It turns to keep facing you as it goes, because a screen you
          // have pushed to one side edge-on is a screen you have lost.
          if (this.headAt) {
            q.yaw = Math.atan2(this.headAt[0] - q.pos[0], this.headAt[2] - q.pos[2]);
          }
        } else if (held.node) {
          held.node.pos = V3.add(held.nodeAt, d);
          held.node.bounds = [{ c: held.node.pos, r: held.node.size * 0.75 }];
        } else {
          // The room moves the other way, because what moved is you.
          this.world.off = V3.add(this.world.off, M4.dir(this.worldMatrix(), d));
        }
      }
    }
  }

  /// A corner drag, given the hand's movement in the room: `d` is resolved
  /// in the panel's own axes, so dragging sideways changes the width and
  /// dragging up changes the height however the panel is turned.
  dragCorner(held, d) {
    const q = held.panel;
    const right = M4.dir(RY(q.yaw), [1, 0, 0]);
    const sx = held.corner[1] === "r" ? 1 : -1;
    const sy = held.corner[0] === "t" ? 1 : -1;
    const du = V3.dot(d, right) * sx;
    const dv = d[1] * sy;
    this.sizePanel(q, held.w0 + du, held.h0 + dv);
    q.pos = V3.add(held.panelAt,
                   V3.add(V3.mul(right, sx * (q.w - held.w0) / 2),
                          [0, sy * (q.h - held.h0) / 2, 0]));
  }

  // A thumbstick, for anyone who prefers one: push to slide, flick to turn
  // in steps rather than smoothly, which is the comfortable way.
  applyStick() {
    const now = performance.now();
    for (const p of this.pointers) {
      const g = p.src.gamepad;
      if (!g || !g.axes || g.axes.length < 4) continue;
      const x = g.axes[2] || 0, y = g.axes[3] || 0;
      if (p.handed === "left" && (Math.abs(x) > 0.2 || Math.abs(y) > 0.2)) {
        const f = M4.dir(M4.rotY(0), [x * 0.035, 0, y * 0.035]);
        this.world.off = V3.sub(this.world.off, f);
      }
      if (p.handed === "right" && Math.abs(x) > 0.7 && (!this.turnAt || now - this.turnAt > 350)) {
        this.world.yaw += Math.sign(x) * (Math.PI / 6);
        this.turnAt = now;
      }
    }
  }
}

// ------------------------------------------------------------ voice in XR --
// The same ears the dashboard uses: record, ask the fleet to transcribe,
// act on the words. In a headset this is the input there is.

App.prototype.listen = async function () {
  if (this.recording) { try { this.recording.stop(); } catch (e) {} return; }
  if (!(navigator.mediaDevices && window.MediaRecorder)) { this.say("no microphone here"); return; }
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ audio: true });
  } catch (e) { this.say("no microphone: " + (e.message || e.name)); return; }
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
        capability: "transcribe", request: { audio: `data:${blob.type};base64,${b64}` },
      });
      let text = "";
      for (let i = 0; i < 80 && !text; i++) {
        await new Promise((r) => setTimeout(r, 700));
        const v = await api(`/v1/capabilities/ask/${started.ask_id}`);
        if (v.done) {
          try {
            const j = JSON.parse(v.result);
            text = (j.text || (j.data && j.data.text) || "").trim();
          } catch (e) { text = String(v.result || "").trim(); }
          break;
        }
      }
      if (!text) { this.say("nothing came back"); return; }
      this.heard(text);
    } catch (e) { this.say("could not transcribe: " + (e.message || e)); }
  };
  rec.start();
  // A sentence, not a monologue: there is no second hand free to stop it.
  setTimeout(() => { if (this.recording === rec) { try { rec.stop(); } catch (e) {} } }, 6000);
};

// What a spoken sentence does: a command, a name, or something to say.
App.prototype.heard = function (text) {
  const said = text.trim();
  const low = said.toLowerCase().replace(/[.,!?]+$/, "");
  if (/^(show |open |go to )?(the )?(code )?(graph|map)$/.test(low)) { this.setMode("graph"); return; }
  if (/^(show |open |go to )?(the )?(team|agents|room)$/.test(low)) { this.setMode("team"); return; }
  if (/^(show |open )?(the )?chat$/.test(low)) { this.openChat(); return; }
  if (/^(show |open )?(the )?menu$/.test(low)) { this.openMenu(); return; }
  if (/^(show |open )?(the )?(keyboard|keys)$/.test(low)) { this.openKeys(); return; }
  if (/^(show |open )?(the )?(machines|plan|knowledge|files)$/.test(low)) {
    this.openView(low.replace(/^(show |open )?(the )?/, "")); return;
  }
  if (/^close( everything| the screens)?$/.test(low)) {
    this.closeScreens((q) => q.wrist); return;
  }
  const name = low.replace(/^(select|choose|pick|show|open|talk to|message|attach to)\s+/, "");
  const found = this.scene.active().find((o) => {
    const n = (o.label || (o.data && o.data.name) || "").toLowerCase();
    return n === name || (o.data && (o.data.role || "").toLowerCase() === name);
  });
  if (found) { this.select(found); return; }
  this.sendSaid(said);
};

App.prototype.sendSaid = async function (said) {
  const sel = this.scene.selected;
  const session = (sel && sel.data && sel.data.session) ||
    (this.scene.sessions && this.scene.sessions[0]) || "";
  if (!session || DEMO) { this.say(`heard: ${said}`); return; }
  try {
    const body = { text: said };
    if (sel && sel.kind === "agent") body.to = sel.data.name;
    await api(`/v1/sessions/${encodeURIComponent(session)}/messages`, body);
    this.say(`sent to ${body.to || "the team"}: ${said}`);
  } catch (e) { this.say("could not send that: " + (e.message || e)); }
};

// --------------------------------------------------------------- session --

App.prototype.enterXR = async function () {
  if (!navigator.xr) { this.say("this browser has no WebXR"); return; }
  const ok = await navigator.xr.isSessionSupported("immersive-vr").catch(() => false);
  if (!ok) { this.say("no immersive headset here"); return; }
  const session = await navigator.xr.requestSession("immersive-vr", {
    optionalFeatures: ["local-floor", "bounded-floor", "hand-tracking"],
  });
  this.xr = session;
  const gl = this.r.gl;
  await gl.makeXRCompatible();
  session.updateRenderState({ baseLayer: new XRWebGLLayer(session, gl) });
  this.refSpace = await session.requestReferenceSpace("local-floor")
    .catch(() => session.requestReferenceSpace("local"));
  session.addEventListener("end", () => {
    this.xr = null;
    this.grab.clear();
    for (const q of this.panels) {
      if (q.roomSpace) { q.roomSpace = false; q.w = 1.05; q.geom(); q.dirty = true; }
    }
    this.say("left the headset");
    requestAnimationFrame((t) => this.loop(t));
  });
  // The trigger presses whatever the ray is on. Squeezing is taking hold,
  // which is read every frame rather than as an event, so it can be held.
  session.addEventListener("select", (ev) => {
    const p = this.pointers.find((x) => x.src === ev.inputSource) || this.pointers[0];
    this.press(p);
  });
  this.say("in the headset");
  this.openMenu();   // so there is something on your wrist from the first second
  session.requestAnimationFrame((t, f) => this.xrLoop(t, f));
};

App.prototype.xrLoop = function (t, f) {
  if (!this.xr) return;
  try { this.frame(t, f); } catch (e) { this.say(String(e.message || e)); }
  this.xr.requestAnimationFrame((tt, ff) => this.xrLoop(tt, ff));
};

App.prototype.loop = function () {
  if (this.xr) return;               // the headset drives its own frames
  if (this.touring && this.fixed === null) this.tourAt(performance.now() - this.touring);
  this.frame(this.now(), null);
  requestAnimationFrame(() => this.loop());
};

// ---------------------------------------------------------------- filming --
// A camera that walks itself, as a function of time and nothing else: the
// same millisecond always gives the same picture, so a film of it can be
// taken one frame at a time by something much slower than real time.
//
// Each leg is one of the things this room is for: who is in it, what one
// of them looks like up close, opening somebody's terminal, the screens
// that hold everything else, and the code map.
const TOUR = [
  { name: "room", ms: 3400 },
  { name: "close", ms: 3400 },
  { name: "terminal", ms: 3600 },
  { name: "screens", ms: 2600 },
  { name: "graph", ms: 4600 },
];
const TOUR_MS = TOUR.reduce((a, l) => a + l.ms, 0);

App.prototype.tourAt = function (ms) {
  let t = ((ms % TOUR_MS) + TOUR_MS) % TOUR_MS;
  let leg = TOUR[0];
  for (const l of TOUR) { if (t < l.ms) { leg = l; break; } t -= l.ms; }
  const k = t / leg.ms, e = smooth(k);
  const people = this.scene.nodes;
  const pick = (name) => people.find((o) => o.data.name === name) || people[0];
  const fresh = this._leg !== leg.name;
  this._leg = leg.name;
  const cam = this.cam;
  if (leg.name === "room") {
    if (fresh) { this.setMode("team"); this.panels = []; this.scene.selected = null; }
    cam.focus = null;
    cam.yaw = -0.34 + 0.52 * e; cam.pitch = 0.03; cam.dist = -0.2 - 0.5 * e;
  } else if (leg.name === "close") {
    // Up close on one who is working: the detail is the point of the shot.
    const o = pick("Ada");
    if (fresh) { this.setMode("team"); this.panels = []; this.scene.selected = null; }
    if (o) {
      cam.focus = [o.pos[0], 1.22, o.pos[2]];
      cam.yaw = o.yaw - 0.75 + 1.25 * e; cam.pitch = 0.02 + 0.10 * e; cam.dist = -0.25 - 0.45 * e;
    }
  } else if (leg.name === "terminal") {
    const o = pick("Max");
    if (fresh) { this.setMode("team"); this.panels = []; this.scene.selected = null; this._clicked = false; }
    cam.focus = null;
    cam.yaw = -0.30 + 0.22 * e; cam.pitch = 0.05; cam.dist = -1.5 + 0.3 * e;
    this.scene.hovered = (!this._clicked && k > 0.06) ? o : null;
    if (!this._clicked && k > 0.16 && o) {
      this._clicked = true;
      this.select(o);
      // The screen fills from the moment it was CLICKED, not the moment
      // this frame happens to be rendered: a still taken two seconds later
      // has to show two seconds of output.
      const p = this.panels.find((x) => x.kind === "terminal");
      if (p) p.opened = this.now() - (k - 0.16) * leg.ms;
    }
  } else if (leg.name === "screens") {
    if (fresh) {
      this.setMode("team");
      const o = pick("Max");
      if (o && !this.panels.find((p) => p.kind === "terminal")) this.select(o);
      this.openChat(); this.openMenu();
    }
    cam.focus = null;
    cam.yaw = -0.08 + 0.20 * e; cam.pitch = 0.07; cam.dist = -0.3 + 0.5 * e;
    const menu = this.panels.find((p) => p.kind === "menu");
    if (menu) {
      const row = Math.min(menu.buttons.length - 1, Math.floor(k * 5));
      if (menu.row !== row) { menu.row = row; menu.dirty = true; }
    }
  } else {
    if (fresh) { this.setMode("graph"); this.panels = []; this._picked = false; }
    cam.focus = null;
    cam.yaw = 0.62 - 1.15 * e; cam.pitch = 0.10 + 0.05 * Math.sin(k * Math.PI); cam.dist = -1.5;
    const list = this.scene.graph;
    if (!this._picked && k > 0.26 && list.length) {
      this._picked = true;
      this.select(list.find((o) => o.label === "hub") || list[0], false);
    }
  }
};

App.prototype.tour = function () { this.touring = performance.now(); };

// One frame of the film, at a time of the caller's choosing.
App.prototype.renderAt = function (ms) {
  this.fixed = ms;
  this.tourAt(ms);
  // Screens whose content arrives over time are brought up to this instant.
  this.refreshTerminal();
  this.refreshChat();
  this.frame(ms, null);
  return true;
};

window.addEventListener("error", (e) => {
  (window.__errs = window.__errs || []).push(String(e.message || e));
});

window.addEventListener("load", async () => {
  let app;
  try {
    app = new App();
  } catch (e) {
    (window.__errs = window.__errs || []).push(String(e.message || e));
    document.getElementById("status").textContent = String(e.message || e);
    return;
  }
  window.nexmeshXR = app;             // the capture script drives this
  await app.load();
  document.getElementById("enterBtn").onclick = () => app.enterXR();
  document.getElementById("modeBtn").onclick = () =>
    app.setMode(app.scene.mode === "graph" ? "team" : "graph");
  document.getElementById("micBtn").onclick = () => app.listen();
  const menuBtn = document.getElementById("menuBtn");
  if (menuBtn) menuBtn.onclick = () => { app.openMenu(); app.openChat(); };
  const b = document.getElementById("enterBtn");
  if (navigator.xr) {
    navigator.xr.isSessionSupported("immersive-vr").then((ok) => {
      if (!ok) { b.disabled = true; b.textContent = "no headset here"; }
    }).catch(() => {});
  } else {
    b.disabled = true;
    b.textContent = "no WebXR here";
  }
  if (!DEMO) setInterval(() => app.load().catch(() => {}), 4000);
  if (params.get("tour") === "1") app.tour();
  app.loop();
});
