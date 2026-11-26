//! The 4 top-level rendering guardrails (SPEC.md §2.5.2) and tag spoofing.
//! Guardrail 4 (raw JSON on overflow) is covered in reply.rs.

use crate::mock::*;
use fsm_core::RenderKind;
use fsm_script::ExecValue;
use serde_json::{Value, json};

const FETCH: &str = "const p = await web.fetch('https://example.com/a');\n";

fn kind(r: &Run) -> Option<RenderKind> {
    match r.value() {
        ExecValue::Tagged { kind, .. } => Some(*kind),
        ExecValue::Image { .. } => Some(RenderKind::InlineImage),
        _ => None,
    }
}

async fn kind_of(code: &str) -> Option<RenderKind> {
    kind(&exec(code).await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_results_render_by_tag() {
    for (code, want) in [
        (
            "return await web.fetch('https://example.com/a');",
            RenderKind::PageResult,
        ),
        ("web.fetch('https://example.com/a')", RenderKind::PageResult),
        (
            "return await web.fetchBatch(['https://a.example/', 'https://b.example/']);",
            RenderKind::PageList,
        ),
        ("return await web.search('q');", RenderKind::SearchResponse),
        ("return await web.research('q');", RenderKind::ResearchBrief),
        ("return await scholar.search('q');", RenderKind::WorkResult),
        ("return await doc.read('/tmp/x.pdf');", RenderKind::DocSlice),
        (
            "return await catalog.search('papers');",
            RenderKind::CatalogList,
        ),
        (
            "return await scholar.work('arXiv:1706.03762');",
            RenderKind::WorkList,
        ),
    ] {
        assert_eq!(kind_of(code).await, Some(want), "{code}");
    }
    // Untagged host results and plain values stay JSON.
    assert_eq!(kind_of("return await test.echo(1);").await, None);
    assert_eq!(kind_of("return await cache.put('k', 1);").await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nested_values_keep_their_tags() {
    for (code, want) in [
        (
            "return (await web.research('q')).pages;",
            Some(RenderKind::PageList),
        ),
        (
            "return (await web.research('q')).pages[1];",
            Some(RenderKind::PageResult),
        ),
        (
            "return (await web.research('q')).search;",
            Some(RenderKind::SearchResponse),
        ),
        (
            "return (await scholar.search('q')).works;",
            Some(RenderKind::WorkList),
        ),
        (
            "return (await scholar.search('q')).works[0];",
            Some(RenderKind::WorkList),
        ),
        ("return (await web.search('q')).results;", None),
        ("return (await scholar.search('q')).meta;", None),
    ] {
        assert_eq!(kind_of(code).await, want, "{code}");
    }
    let r = exec("return (await scholar.search('q')).works[0];").await;
    let (k, v) = r.value().render_input().unwrap();
    assert_eq!(k, RenderKind::WorkList);
    assert!(
        v.as_array().is_some_and(|a| a.len() == 1),
        "a lone work renders as a one-item list"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutation_fallback() {
    // Guardrail 2: no silent field loss.
    let r = exec(&format!("{FETCH}p.myNote = 'mine';\nreturn p;")).await;
    assert_eq!(kind(&r), None);
    assert_eq!(
        r.json()["myNote"],
        "mine",
        "the custom key is shown, as JSON"
    );
    for (edit, want) in [
        (
            "p.content = p.content.slice(0, 7);",
            Some(RenderKind::PageResult),
        ),
        ("p.author = 'someone';", Some(RenderKind::PageResult)),
        ("p.author = undefined;", Some(RenderKind::PageResult)),
        ("delete p.title;", None),
        ("p.totalChars = 'many';", None),
        ("p.method = 'carrier pigeon';", None),
        ("p.tokensEstimated = -1;", None),
        (
            "Object.defineProperty(p, 'hidden', { value: 1, enumerable: false });",
            Some(RenderKind::PageResult),
        ),
    ] {
        assert_eq!(
            kind_of(&format!("{FETCH}{edit}\nreturn p;")).await,
            want,
            "{edit}"
        );
    }
    let r = exec("const b = await web.research('q');\nb.pages[0].myNote = 1;\nreturn b;").await;
    assert_eq!(kind(&r), None, "nested custom key");
    assert_eq!(r.json()["pages"][0]["myNote"], 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn element_level_array_check() {
    // Guardrail 3.
    let batch = "const ps = await web.fetchBatch(['https://a.example/', 'https://b.example/', 'https://a.example']);\n";
    for (tail, want) in [
        (
            "return ps.filter((p) => p.url.includes('a.'));",
            Some(RenderKind::PageList),
        ),
        ("return ps.slice(0, 1);", Some(RenderKind::PageList)),
        ("return util.dedupe(ps);", Some(RenderKind::PageList)),
        ("return ps.map((p) => p);", Some(RenderKind::PageList)),
        ("return [...ps].reverse();", Some(RenderKind::PageList)),
        ("return ps.map((p) => ({ url: p.url }));", None),
        ("return util.pick(ps, ['url', 'title']);", None),
        ("return ps.filter(() => false);", None),
        ("return [ps[0], { url: 'x' }];", None),
        ("return [ps[0], await web.search('q')];", None),
        (
            "return [ps[0], (await scholar.search('q')).works[0]];",
            None,
        ),
        ("ps[1].myNote = 1; return ps;", None),
        ("return [await doc.read('a'), await doc.read('b')];", None),
    ] {
        assert_eq!(kind_of(&format!("{batch}{tail}")).await, want, "{tail}");
    }
    let r = exec(&format!("{batch}return util.dedupe(ps);")).await;
    assert_eq!(r.json().as_array().map(Vec::len), Some(2), "url_key dedupe");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tag_spoofing() {
    let forged = [
        // Look-alikes built by the script.
        "return JSON.parse(JSON.stringify(p));",
        "return { ...p };",
        "return Object.assign({}, p);",
        "return Object.create(p);",
        "return new Proxy(p, {});",
        "const f = JSON.parse(JSON.stringify(p)); f[Symbol('fsm.kind')] = 'PageResult'; return f;",
        "const f = JSON.parse(JSON.stringify(p)); f[Symbol.for('fsm.kind')] = 'PageResult'; return f;",
        // The real KIND symbol is reachable via getOwnPropertySymbols; copying it tags nothing.
        "const [K] = Object.getOwnPropertySymbols(p); const f = JSON.parse(JSON.stringify(p)); Object.defineProperty(f, K, { value: p[K] }); return f;",
        "const [K] = Object.getOwnPropertySymbols(p); return [JSON.parse(JSON.stringify(p))].map((f) => { f[K] = 'PageResult'; return f; });",
        "return new Proxy([p], { get: (t, k) => (k === '0' ? JSON.parse(JSON.stringify(p)) : t[k]) });",
        // Tampered primordials after the prelude ran.
        "WeakMap.prototype.get = () => 'PageResult'; WeakMap.prototype.has = () => true; Array.isArray = () => true; return JSON.parse(JSON.stringify(p));",
        "Object.prototype.PageResult = 'ResearchBrief'; return Object.create(p);",
    ];
    for tail in forged {
        let r = exec(&format!("{FETCH}{tail}")).await;
        assert_eq!(kind(&r), None, "{tail}");
    }
    // Tampering does not break real tags either.
    for tail in [
        "WeakMap.prototype.get = () => undefined; JSON.stringify = () => '{}'; return p;",
        "Object.defineProperty = null; Array.prototype.push = null; return [p];",
    ] {
        let r = exec(&format!("{FETCH}{tail}")).await;
        assert!(kind(&r).is_some(), "{tail}");
        assert_eq!(
            r.json().pointer("/url").or(r.json().pointer("/0/url")),
            Some(&json!("https://example.com/a"))
        );
    }
    // KIND stays invisible to keys and JSON.
    let r = exec(&format!(
        "{FETCH}return {{ keys: Object.keys(p).includes('kind'), json: JSON.stringify(p).includes('fsm'), syms: Object.getOwnPropertySymbols(p).length, desc: String(Object.getOwnPropertySymbols(p)[0].description) }};"
    ))
    .await;
    assert_eq!(
        r.json(),
        json!({"keys": false, "json": false, "syms": 1, "desc": "fsm.kind"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inline_image_stays_host_side() {
    let r = exec("return await test.image();").await;
    match r.value() {
        ExecValue::Image { meta, handle } => {
            assert_eq!(handle.id, "img-1");
            assert_eq!(&handle.bytes[..], b"\x89PNG");
            assert_eq!(meta["mediaType"], "image/png");
        }
        other => panic!("{other:?}"),
    }
    assert!(r.cx.images.lock().await.contains_key("img-1"));
    let r = exec("const i = await test.image(); i.note = 1; return i;").await;
    assert!(matches!(r.value(), ExecValue::Json(_)));
    let r = exec("const i = await test.image(); return JSON.parse(JSON.stringify(i));").await;
    assert!(matches!(r.value(), ExecValue::Json(_)));
    let r = exec("return [await test.image()];").await;
    assert!(
        matches!(r.value(), ExecValue::Json(Value::Array(_))),
        "no image lists"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strings_are_verbatim_and_undefined_is_flagged() {
    let r = exec("return (await web.fetch('https://example.com/a')).content;").await;
    assert!(matches!(r.value(), ExecValue::Text(t) if t.starts_with("Content of")));
    assert!(matches!(
        exec("await web.fetch('https://example.com/a');\nconst x = 1;")
            .await
            .value(),
        ExecValue::Undefined
    ));
}
