//! Unit tests for ID normalization, lawful URL guardrails, section matching,
//! and JATS / BioC / HTML5 structured parsers (zero network).

use fsm_core::{FulltextOpts, HttpBytes};
use fsm_scholar_fulltext::bioc::parse_bioc_json;
use fsm_scholar_fulltext::html5::{parse_arxiv_html5, parse_generic_html5};
use fsm_scholar_fulltext::ids::{
    is_biorxiv_doi, is_blocked_shadow_url, is_disallowed_arxiv_source_url, is_lawful_candidate_url,
    is_paid_openalex_content_url, is_plos_doi, is_springer_nature_doi, normalize_arxiv_id,
    normalize_doi, normalize_pmcid, parse_paper_id, plos_journal_slug,
};
use fsm_scholar_fulltext::jats::{parse_jats_xml, parse_jats_xml_with_limit};
use fsm_scholar_fulltext::sections::{
    ParsedDocument, ParsedSection, decode_response_text, extract_text_sections, filter_and_budget,
    has_fulltext_article_structure, heading_matches, looks_like_paywall_or_stub, normalize_license,
};
use std::time::Instant;

#[test]
fn id_parsing_covers_all_scholarly_formats() {
    // Bare DOI, doi.org URL, trailing prose punctuation
    let d1 = parse_paper_id("https://doi.org/10.1038/NATURE12373.").unwrap();
    assert_eq!(d1.doi.as_deref(), Some("10.1038/nature12373"));
    assert_eq!(d1.canonical_id, "10.1038/nature12373");
    assert!(!d1.is_arxiv_doi);
    assert!(d1.has_scholarly_pid());

    // Bare publisher URL has_any_id() but NOT has_scholarly_pid() (I8)
    let u1 = parse_paper_id("https://www.nature.com/articles/s41586-020-2649-2").unwrap();
    assert!(u1.has_any_id());
    assert!(!u1.has_scholarly_pid());

    // arXiv new-style, versioned, DataCite DOI, legacy S5 style
    let a1 = parse_paper_id("arXiv:1706.03762v5").unwrap();
    assert_eq!(a1.arxiv.as_deref(), Some("1706.03762"));
    assert_eq!(a1.arxiv_fetch_id(), Some("1706.03762v5"));
    assert_eq!(a1.canonical_id, "arXiv:1706.03762");

    let a2 = parse_paper_id("10.48550/arXiv.1706.03762").unwrap();
    assert_eq!(a2.arxiv.as_deref(), Some("1706.03762"));
    assert!(a2.is_arxiv_doi);
    assert!(a2.non_arxiv_doi().is_none());

    let a3 = parse_paper_id("hep-th/9901001v2").unwrap();
    assert_eq!(a3.arxiv.as_deref(), Some("hep-th/9901001"));
    assert_eq!(a3.arxiv_fetch_id(), Some("hep-th/9901001v2"));

    let a4 = parse_paper_id("https://arxiv.org/pdf/2401.00001v1.pdf").unwrap();
    assert_eq!(a4.arxiv.as_deref(), Some("2401.00001"));
    assert_eq!(a4.arxiv_fetch_id(), Some("2401.00001v1"));

    // PMCID & PMID (and reject PPR* as PMCID)
    let p1 = parse_paper_id("https://pmc.ncbi.nlm.nih.gov/articles/pmc3257301/").unwrap();
    assert_eq!(p1.pmcid.as_deref(), Some("PMC3257301"));
    assert_eq!(normalize_pmcid("PPR1373484"), None);

    let p2 = parse_paper_id("PMID:23903748").unwrap();
    assert_eq!(p2.pmid.as_deref(), Some("23903748"));

    // bioRxiv (both legacy 10.1101 and 2026 openRxiv 10.64898) & medRxiv URL
    let b1 = parse_paper_id("https://www.biorxiv.org/content/10.1101/2020.01.30.927871v2.full.pdf")
        .unwrap();
    assert_eq!(b1.doi.as_deref(), Some("10.1101/2020.01.30.927871"));
    assert_eq!(b1.biorxiv_server_hint, Some("biorxiv"));
    assert!(is_biorxiv_doi("10.64898/2026.08.11.744243"));

    // HAL, Zenodo, OpenAlex
    let h1 = parse_paper_id("https://hal.science/hal-04642650v1").unwrap();
    assert_eq!(h1.hal.as_deref(), Some("hal-04642650"));

    let z1 = parse_paper_id("10.5281/zenodo.10900000").unwrap();
    assert_eq!(z1.zenodo.as_deref(), Some("10900000"));

    let oa1 = parse_paper_id("https://openalex.org/W2741809807").unwrap();
    assert_eq!(oa1.openalex.as_deref(), Some("W2741809807"));

    // Empty rejected
    assert!(parse_paper_id("   ").is_err());

    // Helper checks
    assert_eq!(
        normalize_doi("doi:10.1371/journal.pgen.1002441"),
        Some("10.1371/journal.pgen.1002441".into())
    );
    assert_eq!(
        normalize_arxiv_id("math.GT/0309136"),
        Some(("math.gt/0309136".into(), "math.gt/0309136".into()))
    );
    assert!(is_plos_doi("10.1371/journal.pgen.1002441"));
    assert_eq!(
        plos_journal_slug("10.1371/journal.pgen.1002441"),
        "plosgenetics"
    );
    assert_eq!(plos_journal_slug("10.1371/journal.pone.0029797"), "plosone");
    assert!(is_springer_nature_doi("10.1186/s13059-024-03199-x"));
}

#[test]
fn shadow_libraries_paid_openalex_and_arxiv_eprint_are_blocked() {
    for bad in [
        "https://sci-hub.se/10.1038/nature12373",
        "https://libgen.is/scimag/10.1038/nature12373",
        "https://annas-archive.org/scidb/10.1038/nature12373",
        "https://z-lib.gs/book/123",
    ] {
        assert!(is_blocked_shadow_url(bad), "should block {bad}");
        assert!(!is_lawful_candidate_url(bad), "should refuse {bad}");
        assert!(parse_paper_id(bad).is_err(), "direct shadow URL must fail");
    }

    assert!(is_paid_openalex_content_url(
        "https://api.openalex.org/content/W123.pdf"
    ));
    assert!(!is_lawful_candidate_url(
        "https://api.openalex.org/content/W123.pdf"
    ));

    assert!(is_disallowed_arxiv_source_url(
        "https://arxiv.org/e-print/1706.03762"
    ));
    assert!(is_disallowed_arxiv_source_url(
        "https://arxiv.org/src/1706.03762"
    ));
    assert!(!is_lawful_candidate_url(
        "https://arxiv.org/e-print/1706.03762"
    ));

    assert!(is_lawful_candidate_url(
        "https://arxiv.org/html/2401.00001v1"
    ));
    assert!(is_lawful_candidate_url("https://arxiv.org/pdf/1706.03762"));
    assert!(!is_lawful_candidate_url("file:///etc/passwd"));
}

#[test]
fn jats_parser_handles_dtd_named_entities_tables_math_and_subsections() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE article PUBLIC "-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.2//EN" "JATS-archivearticle1.dtd">
<article xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:mml="http://www.w3.org/1998/Math/MathML">
  <front>
    <article-meta>
      <title-group><article-title>Alpha &amp; &beta; Folding &mdash; A Benchmark</article-title></title-group>
      <permissions>
        <license xlink:href="https://creativecommons.org/licenses/by/4.0/"><license-p>CC BY 4.0</license-p></license>
      </permissions>
      <abstract><p>Protein folding accuracy improves to &ge; 95&nbsp;% with &Delta;G &le; 1.2.</p></abstract>
    </article-meta>
  </front>
  <body>
    <sec sec-type="intro">
      <title>1. Introduction</title>
      <p>Folding energy <inline-formula><tex-math>$E = -k_B T \ln Z$</tex-math></inline-formula> governs stability.</p>
    </sec>
    <sec sec-type="methods">
      <title>2. Materials and Methods</title>
      <sec><title>2.1 Dataset Curation</title><p>We filtered 4,000 chains at 30% sequence identity.</p></sec>
      <sec><title>2.2 Statistical Analysis</title><p>Wilcoxon signed-rank tests used &alpha; = 0.01.</p></sec>
    </sec>
    <sec sec-type="results">
      <title>3. Results</title>
      <p>Model A outperformed baseline across every fold class.</p>
      <table-wrap><label>Table 1</label><caption><p>Accuracy summary</p></caption>
        <table><thead><tr><th>Model</th><th>TM-score</th></tr></thead>
        <tbody><tr><td>Baseline</td><td>0.74</td></tr><tr><td>Ours</td><td>0.93</td></tr></tbody></table>
      </table-wrap>
    </sec>
  </body>
  <back>
    <ack><p>Funded by Open Science Grant 42.</p></ack>
    <ref-list><ref><label>1.</label><mixed-citation>Jumper et al. Nature 2021.</mixed-citation></ref></ref-list>
  </back>
</article>"#;

    let doc = parse_jats_xml(xml).unwrap();
    assert_eq!(
        doc.title.as_deref(),
        Some("Alpha & β Folding — A Benchmark")
    );
    assert_eq!(doc.license.as_deref(), Some("CC-BY-4.0"));
    assert!(doc.sections.len() >= 5);
    assert!(doc.sections[0].text.contains("≥ 95 %"));
    assert!(doc.sections[1].text.contains("$E = -k_B T \\ln Z$"));
    assert!(doc.sections[3].text.contains("| Model | TM-score |"));

    // Section filter: "methods" matches "2. Materials and Methods" including nested subsections!
    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["methods".to_owned()]);
    let (secs, md, matched) = filter_and_budget(&doc, &opts, 50_000);
    assert!(matched && secs.len() == 1);
    assert_eq!(secs[0].heading, "2. Materials and Methods");
    assert!(secs[0].text.contains("Dataset Curation") && secs[0].text.contains("α = 0.01"));
    assert!(md.contains("## 2. Materials and Methods"));

    // Subsection-only query: "statistical analysis" resolves the nested subsection!
    let mut sub_opts = FulltextOpts::default();
    sub_opts.sections = Some(vec!["statistical analysis".to_owned()]);
    let (sub_secs, _, sub_matched) = filter_and_budget(&doc, &sub_opts, 50_000);
    assert!(sub_matched && sub_secs.len() == 1);
    assert_eq!(sub_secs[0].heading, "2.2 Statistical Analysis");

    // Unmatched query reports available headings for 1-retry self-correction
    let mut miss_opts = FulltextOpts::default();
    miss_opts.sections = Some(vec!["ethics statement".to_owned()]);
    let (miss_secs, miss_md, miss_matched) = filter_and_budget(&doc, &miss_opts, 50_000);
    assert!(!miss_matched && miss_secs.is_empty());
    assert!(
        miss_md.contains("available sections:") && miss_md.contains("2. Materials and Methods")
    );
}

#[test]
fn bioc_parser_groups_passages_and_titles_from_array_or_object() {
    let bioc = r#"[{
      "source": "PMC",
      "documents": [{
        "id": "3257301",
        "passages": [
          {"infons": {"section_type": "TITLE", "type": "front", "license": "https://creativecommons.org/licenses/by/4.0/"}, "text": "BioC Sample Paper"},
          {"infons": {"section_type": "ABSTRACT", "type": "abstract"}, "text": "Abstract paragraph one."},
          {"infons": {"section_type": "INTRO", "type": "title_1"}, "text": "Introduction"},
          {"infons": {"section_type": "INTRO", "type": "paragraph"}, "text": "Intro paragraph."},
          {"infons": {"section_type": "METHODS", "type": "title_1"}, "text": "Experimental Procedures"},
          {"infons": {"section_type": "METHODS", "type": "title_2"}, "text": "Microscopy"},
          {"infons": {"section_type": "METHODS", "type": "paragraph"}, "text": "Confocal imaging at 63x."},
          {"infons": {"section_type": "REF", "type": "ref"}, "text": "Smith 2020. J Biol."}
        ]
      }]
    }]"#;

    let doc = parse_bioc_json(bioc).unwrap();
    assert_eq!(doc.title.as_deref(), Some("BioC Sample Paper"));
    assert_eq!(doc.license.as_deref(), Some("CC-BY-4.0"));
    assert_eq!(doc.sections.len(), 4);
    assert_eq!(doc.sections[0].heading, "Abstract");
    assert_eq!(doc.sections[1].heading, "Introduction");
    assert_eq!(doc.sections[2].heading, "Experimental Procedures");
    assert!(doc.sections[2].text.contains("### Microscopy"));
    assert_eq!(doc.sections[3].heading, "References");

    assert!(heading_matches("Experimental Procedures", "methods"));
    assert!(heading_matches("3. Materials & Methods", "method"));
    assert!(heading_matches(
        "4. Conclusions and Future Work",
        "conclusion"
    ));
}

#[test]
fn arxiv_html5_parser_normalizes_mathml_and_rejects_unavailable_stub() {
    let stub = "<html><body><h1>HTML is not available for the source</h1></body></html>";
    assert!(parse_arxiv_html5(stub).is_err());

    let html = r#"<!DOCTYPE html>
<html><body>
  <nav class="ltx_page_navbar">Navigation</nav>
  <article class="ltx_document">
    <h1 class="ltx_title ltx_title_document">Attention Is All You Need</h1>
    <div class="ltx_abstract">
      <h6 class="ltx_title ltx_title_abstract">Abstract</h6>
      <p class="ltx_p">We propose the Transformer based solely on attention mechanisms.</p>
    </div>
    <section class="ltx_section" id="S1">
      <h2 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">1</span>Introduction</h2>
      <div class="ltx_para"><p class="ltx_p">Recurrent models factor computation along symbol positions <math alttext="t \in \{1,\dots,n\}"><mi>t</mi></math>.</p></div>
    </section>
    <section class="ltx_section" id="S3">
      <h2 class="ltx_title ltx_title_section"><span class="ltx_tag ltx_tag_section">3</span>Model Architecture</h2>
      <section class="ltx_subsection" id="S3.SS2">
        <h3 class="ltx_title ltx_title_subsection"><span class="ltx_tag ltx_tag_subsection">3.2</span>Scaled Dot-Product Attention</h3>
        <div class="ltx_para"><p class="ltx_p">Attention is computed as <math display="block" alttext="\mathrm{Attention}(Q,K,V)=\mathrm{softmax}(QK^T/\sqrt{d_k})V"><mi>A</mi></math>.</p></div>
      </section>
    </section>
  </article>
  <footer class="ltx_page_footer">Generated by LaTeXML</footer>
</body></html>"#;

    let doc = parse_arxiv_html5(html).unwrap();
    assert_eq!(doc.title.as_deref(), Some("Attention Is All You Need"));
    assert_eq!(doc.sections.len(), 3);
    assert_eq!(doc.sections[0].heading, "Abstract");
    assert_eq!(doc.sections[1].heading, "1 Introduction");
    assert!(doc.sections[1].text.contains(r"$t \in \{1,\dots,n\}$"));
    assert_eq!(doc.sections[2].heading, "3 Model Architecture");
    assert!(
        doc.sections[2]
            .text
            .contains(r"$$\mathrm{Attention}(Q,K,V)=\mathrm{softmax}(QK^T/\sqrt{d_k})V$$")
    );
    assert!(!doc.sections[2].text.contains("Generated by LaTeXML"));

    // Generic OA HTML5 fallback
    let generic = r#"<html><body><article><h1>Open Paper</h1>
      <section><h2>Abstract</h2><p>This open paper presents a detailed methodology and reproducible results across multiple cohorts.</p></section>
      <section><h2>Methods</h2><p>Samples were sequenced on Illumina NovaSeq with 30x depth coverage and aligned to GRCh38.</p></section>
    </article></body></html>"#;
    let gdoc = parse_generic_html5(generic).unwrap();
    assert_eq!(gdoc.sections.len(), 2);
    assert_eq!(gdoc.sections[1].heading, "Methods");
}

#[test]
fn charset_decoding_paywall_detection_license_and_text_splitting_work() {
    // Latin-1 decoding via Content-Type charset
    let bytes = vec![b'C', b'a', b'f', 0xe9, b' ', b'2', b'0', b'2', b'6'];
    let resp = HttpBytes::new(
        "https://example.org/",
        200,
        vec![(
            "Content-Type".into(),
            "text/plain; charset=iso-8859-1".into(),
        )],
        bytes,
    );
    assert_eq!(decode_response_text(&resp), "Café 2026");

    assert!(looks_like_paywall_or_stub(
        "Not available for public API users."
    ));
    assert!(looks_like_paywall_or_stub(
        "<html>Just a moment... Enable JavaScript and cookies to continue</html>"
    ));
    assert!(!looks_like_paywall_or_stub(
        &"Scientific methods and experimental evaluation. ".repeat(20)
    ));

    assert_eq!(
        normalize_license("http://creativecommons.org/licenses/by-nc/4.0/"),
        Some("CC-BY-NC-4.0".into())
    );
    assert_eq!(normalize_license("cc_by_nc_nd"), Some("CC-BY-NC-ND".into()));

    let plain = "Abstract\nShort summary.\n\n1. Introduction\nBackground text.\n\n2. Methods\nMethod details.";
    let tdoc = extract_text_sections(plain, Some("Title".into()), None);
    assert_eq!(tdoc.sections.len(), 3);
    assert_eq!(tdoc.sections[2].heading, "Methods");
}

#[test]
fn malformed_xml_bioc_and_html_degrade_cleanly_without_panics() {
    // Truncated / malformed XML returns Err without panicking
    assert!(parse_jats_xml("<article><body><p>unclosed").is_err());
    assert!(parse_jats_xml("<html/>").is_err());
    // I1: Unknown named entity is preserved verbatim instead of aborting JATS parse!
    let unk = parse_jats_xml("<article><body><p>&totally_unknown_ent;</p></body></article>")
        .expect("unknown named entity must not fail JATS parsing");
    assert!(unk.sections[0].text.contains("&totally_unknown_ent;"));
    // Empty <article> body returns Err
    assert!(parse_jats_xml("<article><front><article-meta/></front><body/></article>").is_err());

    // BioC single-object form (non-array) and empty-passages error
    let single_bioc = r#"{"documents":[{"id":"PMC1","infons":{"license":"CC0"},"passages":[{"infons":{"section_type":"ABSTRACT","type":"abstract"},"text":"Single object BioC abstract."}]}]}"#;
    let doc = parse_bioc_json(single_bioc).unwrap();
    assert_eq!(doc.license.as_deref(), Some("CC0-1.0"));
    assert_eq!(doc.sections[0].heading, "Abstract");
    assert!(parse_bioc_json(r#"{"documents":[]}"#).is_err());
    assert!(parse_bioc_json("not json").is_err());

    // JATS LaTeX wrapper stripping & uneven table column padding
    let math_tbl = r#"<article><body>
      <sec><title>Equations</title>
        <disp-formula><tex-math>\documentclass{minimal}\begin{document}$$a^2 + b^2 = c^2$$\end{document}</tex-math></disp-formula>
        <table-wrap><table><tr><th>A</th><th>B</th></tr><tr><td>1</td></tr></table></table-wrap>
      </sec>
    </body></article>"#;
    let mdoc = parse_jats_xml(math_tbl).unwrap();
    assert!(mdoc.sections[0].text.contains("$$a^2 + b^2 = c^2$$"));
    assert!(mdoc.sections[0].text.contains("| 1 |  |"));

    // HTML5 <annotation encoding="application/x-tex"> without alttext
    let ann_html = r#"<article class="ltx_document">
      <section class="ltx_section"><h2 class="ltx_title_section">1 Method</h2>
      <p class="ltx_p">Loss <math><semantics><mi>L</mi><annotation encoding="application/x-tex">\mathcal{L}(\theta)</annotation></semantics></math>.</p>
      </section></article>"#;
    let hdoc = parse_arxiv_html5(ann_html).unwrap();
    assert!(hdoc.sections[0].text.contains(r"$\mathcal{L}(\theta)$"));

    // FulltextReport in fsm_core::ext::w3c round-trips camelCase JSON cleanly
    let mut rep =
        fsm_core::ext::w3c::FulltextReport::new(fsm_core::FulltextResult::new("10.1/x", false));
    rep.notes
        .push(fsm_core::ext::w3c::UNPAYWALL_SKIP_NOTE.to_owned());
    rep.steps_tried
        .push("step7:unpaywall:skipped_no_email".to_owned());
    let json = serde_json::to_value(&rep).unwrap();
    assert_eq!(json["stepsTried"][0], "step7:unpaywall:skipped_no_email");
    let back: fsm_core::ext::w3c::FulltextReport = serde_json::from_value(json).unwrap();
    assert_eq!(back, rep);
}

// ---------------------------------------------------------------------------
// Review Round 1 regression tests (B2, I1, I2, I3, I11, M1, M2, M8)
// ---------------------------------------------------------------------------

#[test]
fn review_b2_entity_bomb_rejected_fast_and_external_dtd_and_cap_work() {
    // Reviewer Probe C: 2,169-byte XML entity amplification bomb
    let a = "A".repeat(1000);
    let b = "&a;".repeat(250);
    let body = "&b;".repeat(100);
    let bomb = format!(
        "<?xml version=\"1.0\"?>\
        <!DOCTYPE article [\n  <!ENTITY a \"{a}\">\n  <!ENTITY b \"{b}\">\n]>\
        <article><body><p>{body}</p></body></article>"
    );
    let t0 = Instant::now();
    let res = parse_jats_xml(&bomb);
    let elapsed = t0.elapsed();
    assert!(
        res.is_err(),
        "XML internal DTD entity bomb must be rejected, got {res:?}"
    );
    assert!(
        elapsed.as_millis() < 50,
        "entity bomb rejection must complete in < 50 ms, took {} ms",
        elapsed.as_millis()
    );

    // External DOCTYPE without <!ENTITY still parses cleanly
    let ext_dtd = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE article PUBLIC "-//NLM//DTD JATS (Z39.96) Journal Archiving and Interchange DTD v1.2//EN" "JATS-archivearticle1.dtd">
<article><body><sec><title>Intro</title><p>Valid external DTD document.</p></sec></body></article>"#;
    let ok_doc = parse_jats_xml(ext_dtd).expect("external DTD JATS must still parse");
    assert!(
        ok_doc.sections[0]
            .text
            .contains("Valid external DTD document.")
    );

    // Hard char cap stops extraction cleanly
    let huge_body = format!(
        "<article><body><sec><title>Big</title><p>{}</p></sec></body></article>",
        "word ".repeat(2000)
    );
    let capped = parse_jats_xml_with_limit(&huge_body, 500).unwrap();
    assert!(
        capped.total_text_chars() <= 600,
        "expected capped output, got {} chars",
        capped.total_text_chars()
    );
}

#[test]
fn review_i1_named_and_unknown_entities_do_not_fail_jats() {
    // Reviewer Probe B: Caf&eacute;, &rarr;, &zeta;, and unknown &totally_unknown_ent;
    let xml = r#"<article>
      <front><article-meta><title-group><article-title>Caf&eacute; Chemistry</article-title></title-group></article-meta></front>
      <body>
        <sec><title>Results</title>
          <p>Reaction A &rarr; B at potential &zeta; = 0.42 mV with &ldquo;clean&rdquo; yield &uuml;ber &totally_unknown_ent;.</p>
        </sec>
      </body>
    </article>"#;
    let doc = parse_jats_xml(xml).expect("named/unknown HTML entities must never abort JATS parse");
    assert_eq!(doc.title.as_deref(), Some("Café Chemistry"));
    let txt = &doc.sections[0].text;
    assert!(txt.contains('→'), "missing &rarr; in {txt}");
    assert!(txt.contains('ζ'), "missing &zeta; in {txt}");
    assert!(txt.contains("“clean”"), "missing curly quotes in {txt}");
    assert!(txt.contains("über"), "missing &uuml; in {txt}");
    assert!(
        txt.contains("&totally_unknown_ent;"),
        "unknown entity should be preserved literally in {txt}"
    );
}

#[test]
fn review_i2_arxiv_html5_keeps_sectionless_and_pre_section_paragraphs() {
    // 1. Reviewer Probe D: section-less LaTeXML letter with top-level div.ltx_para
    let sectionless = r#"<!DOCTYPE html>
<html><body><article class="ltx_document">
  <h1 class="ltx_title ltx_title_document">A Short Letter on Primes</h1>
  <div class="ltx_abstract">
    <h6 class="ltx_title ltx_title_abstract">Abstract</h6>
    <p class="ltx_p">A concise proof of a prime gap bound.</p>
  </div>
  <div class="ltx_para" id="p1"><p class="ltx_p">Let p_n denote the n-th prime number in ascending order.</p></div>
  <div class="ltx_para" id="p2"><p class="ltx_p">By sieve estimation, infinitely many gaps satisfy the upper bound.</p></div>
  <section class="ltx_bibliography">
    <h2 class="ltx_title ltx_title_bibliography">References</h2>
    <ul class="ltx_biblist"><li class="ltx_bibitem">Zhang 2014. Annals of Mathematics.</li></ul>
  </section>
</article></body></html>"#;

    let doc = parse_arxiv_html5(sectionless).unwrap();
    let headings: Vec<&str> = doc.sections.iter().map(|s| s.heading.as_str()).collect();
    assert!(
        headings.contains(&"Body"),
        "section-less letter must keep Body section, got {headings:?}"
    );
    let body_sec = doc.sections.iter().find(|s| s.heading == "Body").unwrap();
    assert!(body_sec.text.contains("n-th prime number"));
    assert!(body_sec.text.contains("By sieve estimation"));

    // 2. Unheaded intro paragraphs before the first \section
    let pre_section = r#"<!DOCTYPE html>
<html><body><article class="ltx_document">
  <h1 class="ltx_title ltx_title_document">Pre-Section Paper</h1>
  <div class="ltx_abstract"><p class="ltx_p">Brief abstract.</p></div>
  <div class="ltx_para" id="p1"><p class="ltx_p">This unheaded introductory paragraph precedes section one.</p></div>
  <section class="ltx_section" id="S1">
    <h2 class="ltx_title ltx_title_section"><span class="ltx_tag">1</span>Methods</h2>
    <div class="ltx_para"><p class="ltx_p">Section one methodology details.</p></div>
  </section>
</article></body></html>"#;

    let doc2 = parse_arxiv_html5(pre_section).unwrap();
    let headings2: Vec<&str> = doc2.sections.iter().map(|s| s.heading.as_str()).collect();
    assert_eq!(headings2, vec!["Abstract", "Introduction", "1 Methods"]);
    assert!(
        doc2.sections[1]
            .text
            .contains("unheaded introductory paragraph")
    );
}

#[test]
fn review_i3_paywall_markers_at_any_length_and_article_structure_gate() {
    // 6,500-char landing page with strong paywall phrases MUST still be rejected
    let long_paywalled = format!(
        "Abstract\n{}\nAccess through your institution or Buy this article to view the full text.\nReferences\n{}",
        "Detailed summary of biomedical results. ".repeat(30),
        "[1] Author et al. Nature 2020. ".repeat(180)
    );
    assert!(long_paywalled.chars().count() > 6_000);
    assert!(looks_like_paywall_or_stub(&long_paywalled));

    // DSpace repository handle page (abstract + metadata, >2,500 chars, no canonical body sections)
    let dspace_doc = ParsedDocument {
        title: Some("Repository Handle Item".into()),
        license: None,
        sections: vec![
            ParsedSection {
                heading: "Body".into(),
                text: "URI: http://hdl.handle.net/1234/5678 Collections: Theses ".repeat(35),
                subsections: vec![],
            },
            ParsedSection {
                heading: "Abstract".into(),
                text: "This repository record deposits the abstract and citation metadata. "
                    .repeat(20),
                subsections: vec![],
            },
        ],
    };
    assert!(!has_fulltext_article_structure(&dspace_doc));

    // Genuine article with Introduction + Methods + Results passes structure gate
    let real_doc = ParsedDocument {
        title: Some("Open Access Article".into()),
        license: None,
        sections: vec![
            ParsedSection {
                heading: "Abstract".into(),
                text: "Short abstract summary.".into(),
                subsections: vec![],
            },
            ParsedSection {
                heading: "1. Introduction".into(),
                text: "Background motivation and research questions across multiple cohorts. ".repeat(6),
                subsections: vec![],
            },
            ParsedSection {
                heading: "2. Methods".into(),
                text: "Measurements were calibrated across 42 stations with cross-validated regression. ".repeat(6),
                subsections: vec![],
            },
        ],
    };
    assert!(has_fulltext_article_structure(&real_doc));
}

#[test]
fn review_i11_default_max_chars_and_joint_sections_content_budget() {
    let big_para = "Experimental measurements across multiple replicates confirm reproducibility. "
        .repeat(650);
    let doc = ParsedDocument {
        title: Some("Large Monograph".into()),
        license: None,
        sections: vec![
            ParsedSection {
                heading: "Abstract".into(),
                text: "Summary of the monograph.".into(),
                subsections: vec![],
            },
            ParsedSection {
                heading: "Introduction".into(),
                text: big_para.clone(),
                subsections: vec![],
            },
            ParsedSection {
                heading: "Methods".into(),
                text: big_para.clone(),
                subsections: vec![],
            },
            ParsedSection {
                heading: "Results".into(),
                text: big_para,
                subsections: vec![],
            },
        ],
    };
    assert!(doc.total_text_chars() > 140_000);

    // Default max_chars = 50,000 -> content capped near 50k AND sections compacted to headings!
    let (secs, content, matched) = filter_and_budget(&doc, &FulltextOpts::default(), 50_000);
    assert!(matched);
    assert!(
        content.chars().count() <= 50_500,
        "content must respect 50k default cap, got {}",
        content.chars().count()
    );
    let total_sec_text: usize = secs.iter().map(|s| s.text.chars().count()).sum();
    assert_eq!(
        total_sec_text, 0,
        "large unfiltered response should compact sections to headings to avoid 2x duplication"
    );
    assert_eq!(
        secs.iter().map(|s| s.heading.as_str()).collect::<Vec<_>>(),
        vec!["Abstract", "Introduction", "Methods", "Results"]
    );

    // Explicit sections filter still returns section text
    let mut filtered_opts = FulltextOpts::default();
    filtered_opts.sections = Some(vec!["methods".into()]);
    let (fsecs, _, fmatched) = filter_and_budget(&doc, &filtered_opts, 50_000);
    assert!(fmatched && fsecs.len() == 1);
    assert!(fsecs[0].text.contains("Experimental measurements"));
}

#[test]
fn review_m1_jats_extracts_body_ref_list_back_sec_fn_group_floats_and_license_prose() {
    let xml = r#"<article>
      <front><article-meta>
        <title-group><article-title>Europe PMC Layout Test</article-title></title-group>
        <permissions>
          <license license-type="open-access">
            <license-p>This is an open-access article distributed under the terms of the Creative Commons Attribution License, which permits unrestricted use.</license-p>
          </license>
        </permissions>
      </article-meta></front>
      <body>
        <sec><title>Introduction</title>
          <p>Main narrative.</p>
          <def-list><def-item><term>CO2</term><def><p>Carbon dioxide</p></def></def-item></def-list>
          <statement><label>Theorem 1</label><p>Convergence holds.</p></statement>
        </sec>
        <sec sec-type="ref-list">
          <ref-list><title>References</title>
            <ref><label>1.</label><mixed-citation>Hall et al. PLoS Pathog 2012.</mixed-citation></ref>
          </ref-list>
        </sec>
      </body>
      <floats-group>
        <fig><label>Figure 1</label><caption><p>Growth curve under elevated CO2.</p></caption></fig>
      </floats-group>
      <back>
        <sec><title>Data Availability</title><p>All datasets are deposited in Zenodo.</p></sec>
        <fn-group><fn><p>Competing interests: none declared.</p></fn></fn-group>
      </back>
    </article>"#;

    let doc = parse_jats_xml(xml).unwrap();
    assert_eq!(doc.license.as_deref(), Some("CC-BY"));
    let all_text: String = doc
        .sections
        .iter()
        .map(|s| format!("{} :: {}", s.heading, s.text))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all_text.contains("Hall et al. PLoS Pathog 2012"),
        "missing body ref-list: {all_text}"
    );
    assert!(
        all_text.contains("**CO2**: Carbon dioxide"),
        "missing def-list: {all_text}"
    );
    assert!(
        all_text.contains("Theorem 1"),
        "missing statement: {all_text}"
    );
    assert!(
        all_text.contains("Growth curve under elevated CO2"),
        "missing floats-group: {all_text}"
    );
    assert!(
        all_text.contains("deposited in Zenodo"),
        "missing back/sec: {all_text}"
    );
    assert!(
        all_text.contains("Competing interests: none declared"),
        "missing fn-group: {all_text}"
    );
}

#[test]
fn review_m2_all_caps_pdf_headings_split_into_sections() {
    // Reviewer Probe L: ALL-CAPS PDF extracted headings
    let text = "ABSTRACT\nWe evaluate neural sparse retrieval on biomedical corpora.\n\n\
                1 INTRODUCTION\nDense retrievers miss rare entity names.\n\n\
                2 METHODS\nWe train a hybrid BM25 and SPLADE encoder with hard negatives.\n\n\
                3 RESULTS\nMRR@10 improved by 6.4 points.";
    let doc = extract_text_sections(text, Some("Sparse Retrieval".into()), None);
    let headings: Vec<&str> = doc.sections.iter().map(|s| s.heading.as_str()).collect();
    assert_eq!(
        headings,
        vec!["ABSTRACT", "INTRODUCTION", "METHODS", "RESULTS"]
    );

    let mut opts = FulltextOpts::default();
    opts.sections = Some(vec!["methods".into()]);
    let (secs, md, matched) = filter_and_budget(&doc, &opts, 50_000);
    assert!(matched && secs.len() == 1);
    assert!(secs[0].text.contains("hybrid BM25 and SPLADE"));
    assert!(md.contains("hybrid BM25 and SPLADE"));
}

#[test]
fn review_m8_license_normalization_canonical_spdx() {
    assert_eq!(normalize_license("CC BY"), Some("CC-BY".into()));
    assert_eq!(normalize_license("cc-by-4.0"), Some("CC-BY-4.0".into()));
    assert_eq!(normalize_license("cc_by_nc_nd"), Some("CC-BY-NC-ND".into()));
    assert_eq!(
        normalize_license("CC BY-NC-ND 4.0"),
        Some("CC-BY-NC-ND-4.0".into())
    );
    assert_eq!(
        normalize_license("Creative Commons Attribution License"),
        Some("CC-BY".into())
    );
    assert_eq!(
        normalize_license(
            "This is an open-access article distributed under the terms of the Creative Commons Attribution License, which permits unrestricted use, distribution, and reproduction in any medium."
        ),
        Some("CC-BY".into())
    );
    assert_eq!(
        normalize_license("Creative Commons Attribution-NonCommercial 4.0 International"),
        Some("CC-BY-NC-4.0".into())
    );
    assert_eq!(
        normalize_license("https://creativecommons.org/licenses/by-sa/3.0/"),
        Some("CC-BY-SA-3.0".into())
    );
    assert_eq!(normalize_license("CC0"), Some("CC0-1.0".into()));
    assert_eq!(normalize_license("mit"), Some("MIT".into()));
    assert_eq!(normalize_license("apache 2.0"), Some("Apache-2.0".into()));
}

#[test]
fn deeply_nested_html5_degrades_without_stack_overflow() {
    let handle = std::thread::Builder::new()
        .name("html5-depth-guard".into())
        .stack_size(2 << 20)
        .spawn(|| {
            let depth = 10_000;
            let mut html = String::with_capacity(depth * 11 + 256);
            html.push_str("<!doctype html><html><body><article class=\"ltx_document\">");
            for _ in 0..depth {
                html.push_str("<div>");
            }
            html.push_str("<h1>Deep Paper</h1><p>Readable fulltext content that survives extreme nesting safely.</p>");
            for _ in 0..depth {
                html.push_str("</div>");
            }
            html.push_str("</article></body></html>");

            let arxiv = parse_arxiv_html5(&html).expect("arxiv fallback must succeed");
            assert!(
                arxiv
                    .sections
                    .iter()
                    .any(|s| s.text.contains("Readable fulltext content"))
            );
            let generic = parse_generic_html5(&html).expect("generic fallback must succeed");
            assert!(
                generic
                    .sections
                    .iter()
                    .any(|s| s.text.contains("Readable fulltext content"))
            );
        })
        .expect("spawn thread");
    handle.join().expect("thread must not overflow stack");
}

#[test]
fn deeply_nested_jats_xml_terminates_without_stack_overflow() {
    let handle = std::thread::Builder::new()
        .name("jats-depth-guard".into())
        .stack_size(2 << 20)
        .spawn(|| {
            let depth = 4_000;
            // 1. Deeply nested <sec> tree
            let mut sec_xml = String::with_capacity(depth * 11 + 256);
            sec_xml.push_str("<article><body><sec><title>Intro</title><p>Top level paragraph.</p>");
            for _ in 0..depth {
                sec_xml.push_str("<sec>");
            }
            sec_xml.push_str("<p>Deep leaf</p>");
            for _ in 0..depth {
                sec_xml.push_str("</sec>");
            }
            sec_xml.push_str("</sec></body></article>");
            let parsed_sec = parse_jats_xml(&sec_xml).expect("deep <sec> JATS must parse");
            assert!(
                parsed_sec
                    .sections
                    .iter()
                    .any(|s| s.text.contains("Top level paragraph."))
            );

            // 2. Deeply nested inline <italic> tree
            let mut inline_xml = String::with_capacity(depth * 17 + 256);
            inline_xml.push_str("<article><body><sec><title>Body</title><p>Lead ");
            for _ in 0..depth {
                inline_xml.push_str("<italic>");
            }
            inline_xml.push_str("deep");
            for _ in 0..depth {
                inline_xml.push_str("</italic>");
            }
            inline_xml.push_str("</p></sec></body></article>");
            let parsed_inline = parse_jats_xml(&inline_xml).expect("deep inline JATS must parse");
            assert!(
                parsed_inline
                    .sections
                    .iter()
                    .any(|s| s.text.contains("Lead"))
            );
        })
        .expect("spawn thread");
    handle.join().expect("thread must not overflow stack");
}
