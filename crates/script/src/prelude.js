// fsm-script prelude (SPEC.md §2.5.2, §3.1). Rust appends "(<bindings>);" and
// evaluates this once per exec, before the script. It captures and deletes the
// native entry points, installs the frozen host namespaces, console,
// setTimeout/clearTimeout and HostError, and returns the private internals
// object to Rust. Nothing else it creates is reachable from the script.
(function (BINDINGS) {
  "use strict";
  const G = globalThis;
  const hostCall = G.__fsm_host;
  const logLine = G.__fsm_log;
  const urlKey = G.__fsm_url_key;
  const payloadCap = typeof G.__fsm_payload_bytes === "number" ? G.__fsm_payload_bytes : (4 << 20);
  const consoleCap = typeof G.__fsm_console_bytes === "number" ? G.__fsm_console_bytes : 2048;
  delete G.__fsm_host;
  delete G.__fsm_log;
  delete G.__fsm_url_key;
  delete G.__fsm_payload_bytes;
  delete G.__fsm_console_bytes;

  // ---- primordials: captured before any script code can tamper with them ----
  const uncurry = Function.prototype.bind.bind(Function.prototype.call);
  const ObjectFreeze = Object.freeze;
  const ObjectIsExtensible = Object.isExtensible;
  const getOwnPropertyDescriptor = Object.getOwnPropertyDescriptor;
  const defineProperty = Object.defineProperty;
  const hasOwn = uncurry(Object.prototype.hasOwnProperty);
  const isArray = Array.isArray;
  const jsonStringify = JSON.stringify;
  const jsonParse = JSON.parse;
  const arrayPush = uncurry(Array.prototype.push);
  const arrayJoin = uncurry(Array.prototype.join);
  const strCharCodeAt = uncurry(String.prototype.charCodeAt);
  const strIncludes = uncurry(String.prototype.includes);
  const strSlice = uncurry(String.prototype.slice);
  const strToLowerCase = uncurry(String.prototype.toLowerCase);
  const strSplit = uncurry(String.prototype.split);
  const regexpTest = uncurry(RegExp.prototype.test);
  const WeakMapCtor = WeakMap;
  const wmGet = uncurry(WeakMap.prototype.get);
  const wmSet = uncurry(WeakMap.prototype.set);
  const SetCtor = Set;
  const setHas = uncurry(Set.prototype.has);
  const setAdd = uncurry(Set.prototype.add);
  const setDelete = uncurry(Set.prototype.delete);
  const PromiseCtor = Promise;
  const ORIGINAL_THEN = Promise.prototype.then;
  const promiseThen = uncurry(ORIGINAL_THEN);
  const SymbolSpecies = Symbol.species;
  const SAFE_SPECIES_CTOR = ObjectFreeze({ __proto__: null, [SymbolSpecies]: PromiseCtor });
  const ErrorCtor = Error;
  const TypeErrorCtor = TypeError;
  const RegExpCtor = RegExp;
  const StringCtor = String;
  const NumberCtor = Number;
  const NumberIsFinite = Number.isFinite;
  const MathFloor = Math.floor;
  const MathMin = Math.min;
  const MathMax = Math.max;
  const LINE_BREAK = /\r?\n/;
  // http(s) or scheme-relative, leading whitespace allowed (url_key trims and
  // resolves "//host" as https).
  const HTTP_URL = /^\s*(?:https?:)?\/\//i;

  // ---- S1 render tags (guardrail 1) ----
  // KIND is the spec's private, non-enumerable marker. A symbol-keyed property
  // is still reachable through Object.getOwnPropertySymbols, so the authority
  // is TAGS, a closure-private WeakMap: copying KIND onto a forged object, or
  // building a look-alike, tags nothing.
  const KIND = Symbol("fsm.kind");
  const TAGS = new WeakMapCtor();
  const IMAGES = new WeakMapCtor();
  const HOST_ERRORS = new WeakMapCtor();
  // Host list tags tag their elements; a bare element renders as a one-item list.
  const LIST_ITEM = { __proto__: null, PageList: "PageResult", WorkList: "Work", CatalogList: "CatalogHit" };
  const ITEM_LIST = { __proto__: null, PageResult: "PageList", Work: "WorkList", CatalogHit: "CatalogList" };
  const SINGLE = { __proto__: null, Work: "WorkList", CatalogHit: "CatalogList" };
  // Tagged values nested in host results, so `.pages`, `.works`, `.search` keep rendering.
  const NESTED = {
    __proto__: null,
    ResearchBrief: [["search", "SearchResponse", false], ["pages", "PageResult", true]],
    WorkResult: [["works", "Work", true]],
    CitationGraph: [["paper", "Work", false], ["references", "Work", true], ["citations", "Work", true]],
  };

  const mark = (o, tag) => {
    if (o === null || typeof o !== "object" || isArray(o)) return;
    wmSet(TAGS, o, tag);
    defineProperty(o, KIND, { value: tag, enumerable: false, writable: false, configurable: false });
    const nested = NESTED[tag];
    if (nested === undefined) return;
    for (let i = 0; i < nested.length; i++) {
      const spec = nested[i];
      const child = o[spec[0]];
      if (!spec[2]) mark(child, spec[1]);
      else if (isArray(child)) for (let j = 0; j < child.length; j++) mark(child[j], spec[1]);
    }
  };

  const adopt = (value, tag, image) => {
    if (typeof tag !== "string") return value;
    const item = LIST_ITEM[tag];
    if (item === undefined) mark(value, tag);
    else if (isArray(value)) for (let i = 0; i < value.length; i++) mark(value[i], item);
    else mark(value, item);
    if (typeof image === "string" && value !== null && typeof value === "object") wmSet(IMAGES, value, image);
    return value;
  };

  const tagOf = (o) => (o !== null && typeof o === "object" ? wmGet(TAGS, o) : undefined);

  // Boundary, guardrails 1 and 3: [renderKind | null, imageId | null, valueToSerialize].
  // An array renders iff it is non-empty and every element carries the same
  // list-able tag; its elements are read exactly once (snapshot), so a Proxy
  // cannot show the check one thing and the serializer another.
  const finalize = (v) => {
    if (v === null || typeof v !== "object") return [null, null, v];
    if (isArray(v)) {
      const n = v.length;
      if (!(n > 0)) return [null, null, v];
      const snap = [];
      let item;
      for (let i = 0; i < n; i++) {
        const e = v[i];
        const t = tagOf(e);
        if (t === undefined || (item !== undefined && t !== item)) return [null, null, v];
        item = t;
        // Define, never assign: an accessor on Array.prototype must not see or
        // replace the element (single-read snapshot).
        defineProperty(snap, i, { value: e, enumerable: true, writable: true, configurable: true });
      }
      const kind = ITEM_LIST[item];
      return kind === undefined ? [null, null, v] : [kind, null, snap];
    }
    const t = tagOf(v);
    if (t === undefined) return [null, null, v];
    const kind = SINGLE[t] !== undefined ? SINGLE[t] : t;
    const image = t === "InlineImage" ? wmGet(IMAGES, v) : undefined;
    return [kind, typeof image === "string" ? image : null, v];
  };

  // ---- host bridge ----
  class HostError extends ErrorCtor {
    constructor(message, kind, site) {
      super(message);
      this.name = "HostError";
      this.kind = kind;
      const stack = site !== undefined ? site.stack : undefined;
      if (typeof stack === "string") {
        const kept = [];
        const lines = strSplit(stack, "\n");
        for (let i = 0; i < lines.length; i++) {
          if (!strIncludes(lines[i], "fsm-prelude.js")) arrayPush(kept, lines[i]);
        }
        this.stack = arrayJoin(kept, "\n");
      }
    }
  }

  const callHost = (name, args) => {
    // Captured synchronously so the stack points at the caller's line.
    const site = new ErrorCtor("host call");
    let payload;
    try {
      payload = jsonStringify(args);
    } catch (e) {
      // Cycles, BigInt: the bridge turns this into an InvalidInput host error.
      payload = "\u0001" + StringCtor(e !== null && typeof e === "object" ? e.message : e);
    }
    let rawPayload = payload === undefined ? "[]" : payload;
    if (rawPayload.length > payloadCap) {
      // Reject oversized arguments before allocating/copying them onto the Rust heap.
      rawPayload = "\u0002" + StringCtor(rawPayload.length);
    }
    // Deliver host replies through a closure-private native callback instead of
    // `await hostCall(...)` so tampering with Promise.prototype.{constructor,then}
    // or Promise[Symbol.species] can never forge envelopes into TAGS/IMAGES.
    return new PromiseCtor((resolve, reject) => {
      const deliver = (raw) => {
        const reply = jsonParse(raw);
        if (hasOwn(reply, "ok") && reply.ok === true) {
          const tag = hasOwn(reply, "tag") ? reply.tag : null;
          const image = hasOwn(reply, "image") ? reply.image : null;
          const value = hasOwn(reply, "value") ? reply.value : undefined;
          resolve(adopt(value, tag, image));
        } else {
          const msg = hasOwn(reply, "message") ? reply.message : "host error";
          const kind = hasOwn(reply, "kind") ? reply.kind : "Internal";
          const slot = hasOwn(reply, "slot") ? reply.slot : -1;
          const err = new HostError(StringCtor(msg), StringCtor(kind), site);
          wmSet(HOST_ERRORS, err, slot);
          reject(err);
        }
      };
      const syncErr = hostCall(name, rawPayload, deliver);
      if (typeof syncErr === "string") {
        deliver(syncErr);
      }
    });
  };

  // ---- prelude-sync helpers (is_prelude_sync = true) ----
  // Mirrors fsm_core::text::estimate_tokens over code points:
  // ceil(ascii / 4) + ceil(non_ascii / 1.5).
  const estimateTokens = (text) => {
    let s;
    if (typeof text === "string") s = text;
    else if (text === undefined || text === null) s = "";
    else if (typeof text === "object") {
      const j = jsonStringify(text);
      s = j === undefined ? "" : j;
    } else s = StringCtor(text);
    let ascii = 0;
    let other = 0;
    for (let i = 0; i < s.length; i++) {
      const c = strCharCodeAt(s, i);
      if (c < 0x80) {
        ascii++;
        continue;
      }
      other++;
      if (c >= 0xd800 && c <= 0xdbff && i + 1 < s.length) {
        const d = strCharCodeAt(s, i + 1);
        if (d >= 0xdc00 && d <= 0xdfff) i++;
      }
    }
    return MathFloor((ascii + 3) / 4) + MathFloor((2 * other + 2) / 3);
  };

  // Default identity: an http(s) URL (or an object's `url`) goes through
  // fsm_core::url_canon::url_key; anything else is its JSON text.
  const dedupeKey = (x) => {
    if (typeof x === "string") return regexpTest(HTTP_URL, x) ? "u:" + urlKey(x) : "s:" + x;
    if (x !== null && typeof x === "object" && typeof x.url === "string" && x.url !== "") {
      return "u:" + urlKey(x.url);
    }
    let j;
    try {
      j = jsonStringify(x);
    } catch (_) {
      j = undefined;
    }
    return "j:" + (j === undefined ? StringCtor(x) : j);
  };

  const dedupe = (items, keyFn) => {
    if (!isArray(items)) throw new TypeErrorCtor("util.dedupe(items, keyFn?): items must be an array");
    if (keyFn !== undefined && typeof keyFn !== "function") {
      throw new TypeErrorCtor("util.dedupe(items, keyFn?): keyFn must be a function");
    }
    const seen = new SetCtor();
    const out = [];
    for (let i = 0; i < items.length; i++) {
      const x = items[i];
      const k = keyFn === undefined ? dedupeKey(x) : "k:" + StringCtor(keyFn(x, i));
      if (setHas(seen, k)) continue;
      setAdd(seen, k);
      arrayPush(out, x);
    }
    return out;
  };

  const textOf = (x) => {
    if (typeof x === "string") return x;
    if (x === undefined || x === null) return "";
    if (typeof x === "object") {
      if (typeof x.content === "string") return x.content;
      if (typeof x.body === "string") return x.body;
      const j = jsonStringify(x, null, 1);
      return j === undefined ? "" : j;
    }
    return StringCtor(x);
  };

  const GREP_MAX = 500;
  const grep = (text, pattern, contextLines) => {
    let test;
    if (pattern instanceof RegExpCtor) {
      test = (line) => {
        pattern.lastIndex = 0;
        return regexpTest(pattern, line);
      };
    } else {
      const needle = strToLowerCase(StringCtor(pattern === undefined || pattern === null ? "" : pattern));
      if (needle === "") throw new TypeErrorCtor("util.grep(text, pattern, contextLines?): pattern is empty");
      test = (line) => strIncludes(strToLowerCase(line), needle);
    }
    const c0 = NumberCtor(contextLines);
    const ctx = NumberIsFinite(c0) ? MathMax(0, MathMin(50, MathFloor(c0))) : 0;
    const lines = strSplit(textOf(text), LINE_BREAK);
    const out = [];
    let start = -1;
    let end = -1;
    const flush = () => {
      if (start < 0) return;
      const chunk = [];
      for (let k = start; k <= end; k++) arrayPush(chunk, lines[k]);
      arrayPush(out, arrayJoin(chunk, "\n"));
    };
    for (let i = 0; i < lines.length && out.length < GREP_MAX; i++) {
      if (!test(lines[i])) continue;
      const s = MathMax(0, i - ctx);
      const e = MathMin(lines.length - 1, i + ctx);
      // With context, touching windows merge (like grep -C); without, each line stands alone.
      if (start >= 0 && s <= end + (ctx > 0 ? 1 : 0)) {
        end = MathMax(end, e);
      } else {
        flush();
        start = s;
        end = e;
      }
    }
    if (out.length < GREP_MAX) flush();
    return out;
  };

  const pickOne = (obj, keys) => {
    if (obj === null || typeof obj !== "object") {
      throw new TypeErrorCtor("util.pick(obj, keys): obj must be an object (or an array of objects)");
    }
    const out = {};
    for (let i = 0; i < keys.length; i++) {
      const k = keys[i];
      // Define, so "__proto__" (or an Object.prototype setter) copies as data.
      if (hasOwn(obj, k)) defineProperty(out, k, { value: obj[k], enumerable: true, writable: true, configurable: true });
    }
    return out;
  };

  const pick = (obj, keys) => {
    const ks = isArray(keys) ? keys : [keys];
    if (!isArray(obj)) return pickOne(obj, ks);
    const out = [];
    for (let i = 0; i < obj.length; i++) arrayPush(out, pickOne(obj[i], ks));
    return out;
  };

  const HELPERS = {
    __proto__: null,
    "util.dedupe": dedupe,
    "util.grep": grep,
    "util.pick": pick,
    "util.estimateTokens": estimateTokens,
  };

  // ---- namespaces from the HOST_API rows ----
  const spaces = { __proto__: null };
  for (let i = 0; i < BINDINGS.length; i++) {
    const row = BINDINGS[i];
    const full = row[0];
    const ns = row[1];
    const fn = row[2];
    let impl;
    if (row[3]) {
      impl = HELPERS[full];
      if (impl === undefined) throw new ErrorCtor("no prelude implementation for " + full);
    } else {
      impl = (...args) => callHost(full, args);
    }
    if (spaces[ns] === undefined) spaces[ns] = {};
    spaces[ns][fn] = impl;
  }
  for (const ns in spaces) {
    if (hasOwn(G, ns)) throw new ErrorCtor("host namespace collides with a global: " + ns);
    defineProperty(G, ns, { value: ObjectFreeze(spaces[ns]), enumerable: false, writable: false, configurable: false });
  }

  // ---- console (captured, capped host-side) ----
  const fmt = (x) => {
    if (typeof x === "string") return x;
    try {
      const s = jsonStringify(x);
      return s === undefined ? StringCtor(x) : s;
    } catch (_) {
      return StringCtor(x);
    }
  };
  let consoleSent = 0;
  const logger = (prefix) => (...args) => {
    if (consoleSent > consoleCap) return;
    const parts = [];
    for (let i = 0; i < args.length; i++) arrayPush(parts, fmt(args[i]));
    let line = prefix + arrayJoin(parts, " ");
    const maxChars = consoleCap + 16;
    if (line.length > maxChars) line = strSlice(line, 0, maxChars);
    consoleSent += line.length + 1;
    logLine(line);
  };
  defineProperty(G, "console", {
    value: ObjectFreeze({
      log: logger(""),
      info: logger(""),
      debug: logger("debug: "),
      warn: logger("warn: "),
      error: logger("error: "),
    }),
    enumerable: false,
    writable: false,
    configurable: false,
  });

  // ---- setTimeout shim (S2): one util.sleep host call per timer ----
  let nextTimer = 1;
  const cleared = new SetCtor();
  const setTimeoutShim = (fn, ms, ...args) => {
    if (typeof fn !== "function") {
      throw new TypeErrorCtor("setTimeout(fn, ms): fn must be a function (string code is not supported)");
    }
    const id = nextTimer++;
    const n = NumberCtor(ms);
    const delay = NumberIsFinite(n) && n > 0 ? n : 0;
    const payload = jsonStringify([delay]);
    const onSleep = (raw) => {
      const reply = jsonParse(raw);
      if (hasOwn(reply, "ok") && reply.ok === true) {
        if (setDelete(cleared, id)) return;
        try {
          fn(...args);
        } catch (e) {
          logLine("error: setTimeout callback threw: " + fmt(e instanceof ErrorCtor ? StringCtor(e) : e));
        }
      } else {
        const msg = hasOwn(reply, "message") ? reply.message : "setTimeout failed";
        logLine("error: setTimeout: " + StringCtor(msg));
      }
    };
    const syncErr = hostCall("util.sleep", payload, onSleep);
    if (typeof syncErr === "string") {
      onSleep(syncErr);
    }
    return id;
  };
  const clearTimeoutShim = (id) => {
    setAdd(cleared, id);
  };
  defineProperty(G, "setTimeout", { value: setTimeoutShim, enumerable: false, writable: true, configurable: true });
  defineProperty(G, "clearTimeout", { value: clearTimeoutShim, enumerable: false, writable: true, configurable: true });
  defineProperty(G, "HostError", { value: HostError, enumerable: false, writable: true, configurable: true });

  // Rust awaits results through rquickjs, which looks `then` up as a property
  // and `ORIGINAL_THEN` consults `this.constructor[Symbol.species]`. Hand it a
  // fresh promise whose own `then` and `constructor` are locked, and shield
  // `promiseThen(p, ...)` from tampered `Promise.prototype.constructor` /
  // `Promise[Symbol.species]` without spawning extra async frames.
  const wrapPromise = (p) => {
    const q = new PromiseCtor((resolve, reject) => {
      let patched = false;
      try {
        if (p !== null && typeof p === "object" && ObjectIsExtensible(p)) {
          if (getOwnPropertyDescriptor(p, "constructor") === undefined) {
            defineProperty(p, "constructor", {
              value: SAFE_SPECIES_CTOR,
              enumerable: false,
              writable: true,
              configurable: true,
            });
            patched = true;
          }
        }
        promiseThen(p, resolve, reject);
      } finally {
        if (patched) {
          try {
            delete p.constructor;
          } catch (_) {}
        }
      }
    });
    defineProperty(q, "then", { value: ORIGINAL_THEN, enumerable: false, writable: false, configurable: false });
    defineProperty(q, "constructor", { value: SAFE_SPECIES_CTOR, enumerable: false, writable: false, configurable: false });
    return q;
  };

  const hostErrorSlot = (e) => {
    const s = e !== null && typeof e === "object" ? wmGet(HOST_ERRORS, e) : undefined;
    return typeof s === "number" ? s : -1;
  };
  return ObjectFreeze({ finalize, hostErrorSlot, wrapPromise });
})
