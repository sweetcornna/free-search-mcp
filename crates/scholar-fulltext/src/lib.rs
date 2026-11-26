//! `fsm-scholar-fulltext` — 11-step structured-first lawful full-text chain
//! (owner: lane `W3c`, `SPEC.md` §6, `academic.md` §11).
//!
//! Resolution order (lawful open-access routes only; shadow libraries are
//! strictly blocked):
//! 1. PMC `idconv` (`DOI`/`PMID` → `PMCID`)
//! 2. Europe PMC JATS XML
//! 3. PMC BioC JSON
//! 4. bioRxiv / medRxiv JATS XML (`.source.xml`)
//! 5. arXiv native HTML5 (`ar5iv` fallback)
//! 6. PLOS / Springer Nature OA JATS XML
//! 7. Unpaywall (skipped cleanly with a note when `SEARCH_MCP_CONTACT_EMAIL` is unset — I16)
//! 8. OpenAlex `$0` `best_oa_location` / Semantic Scholar `openAccessPdf`
//! 9. CORE v3 DOI search (`fullText` / repository PDF)
//! 10. DOAJ / Crossref TDM `link[]` / DataCite / HAL / Zenodo (+ FAIR Signposting)
//! 11. Wayback CDX (last-resort fallback for link-rotted OA URLs)

pub mod bioc;
pub mod chain;
pub mod html5;
pub mod ids;
pub mod jats;
pub mod sections;

pub use chain::ScholarFulltext;
pub use fsm_core::ext::w3c::{FulltextReport, UNPAYWALL_SKIP_NOTE};
