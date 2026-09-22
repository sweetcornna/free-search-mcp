# Keyless data sources: a survey

A list of JSON endpoints that answer a factual question with the record
itself and need no API key, gathered on 2026-09-21 and 2026-09-22 for the
next round of direct-fact engines. About 150 candidate URLs were taken from
the GitHub lists at the end of this page and from the official statistics,
registry and package-index sites, and each was called without a key; "works"
below means that call returned structured data. The exit IP of a probe is not
the exit IP of a deployment, so an endpoint marked "needs UA" or "needs
Referer" should be given that header in an engine rather than assumed to
tolerate its absence.

The sources already implemented as engines before the survey (PyPI, npm,
crates.io, endoflife.date, GitHub releases, NVD, OSV, Wikidata, Open-Meteo,
MDN, IETF Datatracker, Frankfurter, Federal Register, GOV.UK, and the
scholarly, finance, dataset and image sources) are not repeated here. From
the survey itself, these became engines on 2026-09-22: the seven other
package registries (`registries`), the App Store (`appstore`), World Bank
indicators (`wdi`), Nager.Date and holiday-cn (`holidays`), the IANA zone
database (`worldclock`, no network), CFETS central parity (`cfets`), the
open ExchangeRate-API endpoint (inside `frankfurter`), GLEIF (`gleif`), CISA
KEV (`cisakev`), CoinGecko (`coingecko`) and RDAP (`rdap`). Their rows stay
below for the limits and caveats.

## Candidates that work, by how often an agent would need them

| Family | Example request | Answers | Limits, headers, terms | Chinese questions or China data |
|---|---|---|---|---|
| Package registries not yet covered: Maven Central `search.maven.org`, RubyGems `rubygems.org/api/v1`, Packagist `repo.packagist.org/p2`, NuGet `api.nuget.org/v3-flatcontainer`, Go proxy `proxy.golang.org`, Homebrew `formulae.brew.sh/api`, Hex `hex.pm/api`, Docker Hub `hub.docker.com/v2`, jsDelivr `data.jsdelivr.com/v1`, cdnjs `api.cdnjs.com`; cross-ecosystem: deps.dev `api.deps.dev/v3`, Ecosyste.ms `packages.ecosyste.ms/api/v1` | `https://rubygems.org/api/v1/versions/rails/latest.json`, `https://proxy.golang.org/github.com/gin-gonic/gin/@latest`, `https://formulae.brew.sh/api/formula/wget.json` | current version, release time, licence of a library or image | no stated limits; Docker Hub has a per-IP anonymous quota | no regional limit |
| Runtime and browser versions: Node `nodejs.org/dist/index.json`, Chrome `chromiumdash.appspot.com/fetch_releases`, Firefox `product-details.mozilla.org/1.0/firefox_versions.json`, Linux `kernel.org/releases.json`, PHP `php.net/releases/index.php?json`, python.org `/api/v2/downloads/release/`, Rust `static.rust-lang.org/dist/channel-rust-stable.toml` | `https://chromiumdash.appspot.com/fetch_releases?channel=Stable&platform=Windows&num=1` | current stable, LTS, ESR and release dates (the upstreams of endoflife.date) | official static JSON; python.org ignores `limit` and returns the whole table; Rust is an 878 KB TOML | same |
| Exchange rates beyond the ECB: `open.er-api.com/v6`, fawazahmed0 currency-api on jsDelivr, CFETS RMB central parity `chinamoney.com.cn/r/cms/www/chinamoney/data/fx/ccpr.json` and history `/ags/ms/cm-u-bk-ccpr/CcprHisNew` | `https://open.er-api.com/v6/latest/USD`, `https://cdn.jsdelivr.net/npm/@fawazahmed0/currency-api@latest/v1/currencies/cny.json` | a rate on a day; the official RMB central parity | er-api updates once a day and asks for an attribution link, commercial use allowed, resale not; currency-api covers 150+ fiat and crypto currencies daily; CFETS is official JSON with no stated terms | CFETS is the Chinese official source, fields in Chinese |
| Public holidays: Nager.Date `nagerholidays.com/api/v4` (old `date.nager.at/api/v3` still answers), holiday-cn (parsed State Council notices), OpenHolidays (Europe), GOV.UK bank holidays | `https://nagerholidays.com/api/v4/Holidays/CN/2026`, `https://raw.githubusercontent.com/NateScarlet/holiday-cn/master/2026.json`, `https://www.gov.uk/bank-holidays.json` | a country's holidays for a year; China's adjusted working days | Nager states no rate limits, 200+ countries; holiday-cn is served from GitHub raw and links the State Council notice | holiday-cn mirrors the official Chinese schedule including make-up work days |
| DBnomics `api.db.nomics.world/v22` | `https://api.db.nomics.world/v22/series/NBS/A_A0101?limit=2&observations=1` | time series from 93 statistical providers (IMF, OECD, ECB, BIS, World Bank, WHO, Eurostat, BLS, Fed, BoJ, China's NBS) under one query syntax | no key or limit stated; data under each provider's licence | has an NBS provider with 3,150 datasets; `data.stats.gov.cn` itself answers scripts with 403, so this is the working route to NBS yearbook data (series names in English) |
| World Bank `api.worldbank.org/v2` | `https://api.worldbank.org/v2/country/CHN/indicator/NY.GDP.MKTP.CD?format=json&mrv=3` | GDP, population and other indicators with their year | no key, limit or UA stated | all China indicators; `?language=zh` gives some indicator names in Chinese |
| International statistics: Eurostat, OECD SDMX `sdmx.oecd.org/public/rest`, WHO GHO `ghoapi.azureedge.net/api`, UN SDG `unstats.un.org/sdgapi`, UN population `population.un.org/dataportalapi/api/v1`, UN Comtrade preview `comtradeapi.un.org/public/v1/preview` | `https://comtradeapi.un.org/public/v1/preview/C/A/HS?reporterCode=156&period=2023&partnerCode=0&cmdCode=TOTAL` | EU GDP, OECD leading indicators, health indicators, trade values | Comtrade preview: one period per call, 500 rows, about 1 request per second; the full API needs a free key; OECD structure queries reject `format=jsondata` (406), use it on `/data/` only | WHO, OECD and Comtrade cover China (CHN, 156) |
| US, UK and Canadian official economics: Treasury FiscalData `api.fiscaldata.treasury.gov`, BLS v1 `api.bls.gov/publicAPI/v1`, Fed H.15 CSV, ONS `api.beta.ons.gov.uk/v1`, Bank of England IADB CSV, Statistics Canada WDS | `https://api.fiscaldata.treasury.gov/services/api/fiscal_service/v2/accounting/od/debt_to_penny?sort=-record_date&page[size]=1`, `https://api.bls.gov/publicAPI/v1/timeseries/data/LNS14000000` | US public debt, unemployment, Treasury yields, UK Bank Rate | BLS v1 without a key: 25 calls a day, 10 years and 25 series per call; others state no limit | no China data |
| Chinese macro and quotes: East Money data centre `datacenter-web.eastmoney.com/api/data/v1/get`, quotes `push2.eastmoney.com/api/qt/stock/get`, Tencent `qt.gtimg.cn` as a fallback | `https://datacenter-web.eastmoney.com/api/data/v1/get?reportName=RPT_ECONOMY_CPI&columns=ALL&pageSize=3&sortColumns=REPORT_DATE&sortTypes=-1`, `https://push2.eastmoney.com/api/qt/stock/get?secid=1.600519&fields=f43,f57,f58,f169,f170` | monthly CPI, PPI, PMI, GDP; A-share, HK and US quotes (HK `116.00700`, US `105.AAPL`) | unofficial and undocumented; prices are integers scaled by 100 or 1000; Tencent returns a GBK-encoded JS variable; Sina `hq.sinajs.cn` needs a Referer | entirely Chinese |
| Crypto and US equities: CoinGecko `api.coingecko.com/api/v3`, Binance `api.binance.com/api/v3`, Coinpaprika, DefiLlama `coins.llama.fi`, Yahoo Finance `query1.finance.yahoo.com/v8/finance/chart` (unofficial) | `https://api.coingecko.com/api/v3/simple/price?ids=bitcoin&vs_currencies=usd,cny`, `https://api.binance.com/api/v3/ticker/price?symbol=BTCUSDT` | coin prices, US share prices and history | CoinGecko without a key shares a per-IP limit (a demo key gives 100 a minute); Yahoo is unsupported and returns 429 and crumb checks at times | CoinGecko accepts `vs_currencies=cny` |
| Geocoding: Nominatim, Photon `photon.komoot.io/api`, Zippopotam `api.zippopotam.us`, Postcodes.io, French BAN `api-adresse.data.gouv.fr`, Overpass `overpass-api.de` | `https://nominatim.openstreetmap.org/search?q=北京市海淀区&format=jsonv2&limit=1`, `https://api.zippopotam.us/us/90210` | place to coordinates, postcode to town, administrative boundaries | Nominatim policy: at most 1 request a second, an identifying User-Agent or Referer, results must be cached, no autocomplete; Photon asks for reasonable use and promises no availability | Nominatim and Photon take Chinese place names and return Chinese |
| Time and zones: timeapi.io | `https://timeapi.io/api/time/current/zone?timeZone=Asia/Shanghai`, `https://timeapi.io/api/timezone/coordinate?latitude=39.9&longitude=116.4` | the current time in a zone, the zone at a coordinate, DST state | no stated limit; WorldTimeAPI was unreachable throughout the survey | every IANA zone |
| Legal entities: GLEIF LEI `api.gleif.org/api/v1`, EU VIES VAT `ec.europa.eu/taxation_customs/vies/rest-api`, French companies `recherche-entreprises.api.gouv.fr` | `https://api.gleif.org/api/v1/lei-records?filter[entity.legalName]=阿里巴巴&page[size]=1`, `https://ec.europa.eu/taxation_customs/vies/rest-api/ms/DE/vat/811907980` | registered legal name, address, LEI, parent; whether an EU VAT number is valid | GLEIF: 60 requests a minute per user | GLEIF searches Chinese legal names, but covers only entities that hold an LEI (mostly financial institutions and listed companies) |
| Chinese national standards: `std.samr.gov.cn/gb/search/gbQueryPage` | `https://std.samr.gov.cn/gb/search/gbQueryPage?searchText=GB%2FT%201.1&pageNumber=1&pageSize=5` | whether a GB or GB/T standard is current, its effective date, mandatory or recommended | the site's own front-end endpoint, undocumented; keep the rate low | official, Chinese |
| Law and case law: CourtListener v4 `courtlistener.com/api/rest/v4`, EU CELLAR SPARQL `publications.europa.eu/webapi/rdf/sparql`, legislation.gov.uk (XML and Atom) | `https://www.courtlistener.com/api/rest/v4/search/?q=%22fair%20use%22&type=o`, `https://www.legislation.gov.uk/ukpga/2018/12/contents/data.xml` | US case search, EU legislation metadata, UK statute text | CourtListener's examples carry a token but many endpoints answer anonymously; authenticated users get 5 a minute, 50 an hour, 125 a day; legislation.gov.uk has no JSON | no Chinese law; `flk.npc.gov.cn/api/` is a POST interface and a GET returns an HTML shell, so it was not verified |
| W3C `api.w3.org` | `https://api.w3.org/specifications/html5` | a specification's status, latest version and working group | documented as public with no authentication; 6,000 requests per IP per 10 minutes | none |
| Security beyond NVD and OSV: CVE.org `cveawg.mitre.org/api/cve`, CISA KEV JSON, Ubuntu CVE `ubuntu.com/security/cves/{id}.json`, GitHub Advisories `api.github.com/advisories` | `https://cveawg.mitre.org/api/cve/CVE-2024-3094`, `https://www.cisa.gov/sites/default/files/feeds/known_exploited_vulnerabilities.json` | the CNA record, whether a CVE is exploited in the wild, a distribution's fix status | GitHub anonymous: 60 an hour and a User-Agent required | none |
| Domains and networks: RDAP at the registry (`rdap.verisign.com/com/v1/domain/`), Google DoH `dns.google/resolve`, RIPEstat `stat.ripe.net/data`, ipapi.co, crt.sh | `https://rdap.verisign.com/com/v1/domain/example.com`, `https://dns.google/resolve?name=example.com&type=A`, `https://stat.ripe.net/data/network-info/data.json?resource=8.8.8.8` | registration and expiry dates, registrar, DNS answers, the AS of an IP, certificate transparency | RIPEstat: unlimited, 8 concurrent per IP, add `sourceapp` above 1,000 a day; `rdap.org` answered the probe with 403, so follow the IANA bootstrap to the registry; crt.sh timed out or returned 502 often | ipapi.co geolocates Chinese IPs; ip-api.com's free tier is HTTP only |
| Wayback availability `archive.org/wayback/available` | `https://archive.org/wayback/available?url=example.com&timestamp=20200101` | whether a snapshot near a date exists, and its URL, to check what a page used to say | no key; the CDX endpoint was blocked by the probe tool and not tested | Chinese sites are archived too |
| Medicine and chemistry: openFDA `api.fda.gov`, RxNorm `rxnav.nlm.nih.gov/REST`, NLM Clinical Tables (ICD-10), PubChem PUG REST, ChEMBL | `https://api.fda.gov/drug/label.json?search=openfda.brand_name:%22Tylenol%22&limit=1`, `https://pubchem.ncbi.nlm.nih.gov/rest/pug/compound/name/aspirin/property/MolecularFormula,MolecularWeight,IUPACName/JSON` | drug labels and recalls, ICD codes, molecular formula and weight | openFDA without a key: 240 a minute and 1,000 a day per IP | English only |
| Scholarly registries: ROR `api.ror.org/v2`, ORCID public API, DataCite `api.datacite.org`, Unpaywall, OpenCitations `api.opencitations.net`, bioRxiv `api.biorxiv.org` | `https://api.ror.org/v2/organizations?query=tsinghua`, `https://api.unpaywall.org/v2/10.1038/nature12373?email=you@yourdomain`, `https://api.opencitations.net/index/v2/citation-count/doi:10.1002/adfm.201505328` | institution identifiers, author ORCIDs, dataset DOIs, open-access PDF links, citation counts, preprints | ORCID returns XML unless `Accept: application/json`; Unpaywall rejects `test@example.com`; the old OpenCitations domain redirects | ROR covers Chinese universities and institutes |
| Earth and weather agencies: USGS earthquakes `earthquake.usgs.gov/fdsnws`, US NWS `api.weather.gov`, Hong Kong Observatory `data.weather.gov.hk/weatherAPI`, JMA `jma.go.jp/bosai`, Singapore `api-open.data.gov.sg/v2`, NOAA tides and space weather | `https://earthquake.usgs.gov/fdsnws/event/1/query?format=geojson&limit=2&minmagnitude=5`, `https://data.weather.gov.hk/weatherAPI/opendata/weather.php?dataType=rhrread&lang=tc` | recent earthquakes, official observations and forecasts, tides, Kp index | NWS requires a User-Agent | HKO answers in traditional Chinese; CEIC (China Earthquake Networks) has an expired certificate and 404s; NMC `rest/real/54511` returned empty data (a Referer may be needed); the NMC typhoon list works but is JSONP |
| Transport: TfL `api.tfl.gov.uk`, OpenSky `opensky-network.org/api`, adsb.lol `api.adsb.lol/v2`, db.transport.rest | `https://api.tfl.gov.uk/Line/victoria/Status` | London line status, aircraft in a bounding box | OpenSky anonymous: 400 credits a day, 10 s resolution, latest state only; db.transport.rest states 100 a minute and returned 503 during the survey | 12306 needs a login cookie; no Chinese transport API was usable |
| Culture: Apple iTunes lookup, MusicBrainz, TVmaze, Gutendex, Library of Congress `loc.gov/?fo=json`, Nobel Prize `api.nobelprize.org/2.1` | `https://itunes.apple.com/lookup?id=414478124&country=cn`, `https://api.nobelprize.org/2.1/laureates?nobelPrizeYear=2025&limit=3` | an iOS app's current version and release notes, air dates, laureates | MusicBrainz: 1 a second and a `App/version (contact)` User-Agent or 503; Nobel and GovTrack return 403 to a default UA; Google Books returned 429 from a shared exit IP | iTunes takes `country=cn` |

Also verified, lower priority: GBIF species matching, UniProt REST, Ensembl
REST, Launch Library 2, NHTSA recalls and VIN decoding, data.police.uk, Hebcal,
UK carbon intensity, Open Food Facts (needs an `AppName/Version (email)` UA,
15 a minute), Sunrise-Sunset, dictionaryapi.dev, the SPDX licence list,
mledoze/countries (a static stand-in for REST Countries with Chinese country
names, capitals, currencies and calling codes), Coinpaprika, Weibo trending
(needs `Referer: https://weibo.com/`).

## Checked and rejected

| Status | Source | Evidence |
|---|---|---|
| needs a key | REST Countries | v3.1 redirects to a deprecation notice; v5 needs an account and a bearer key |
| needs a key | US Census Bureau API | "Missing Key. A valid key must be included with each data API request"; the old anonymous 500 a day is gone |
| needs a key | OpenCorporates | 401; free tier 200 a month |
| needs a key | UK Companies House | every request must carry credentials |
| needs a key | exchangerate.host | `missing_access_key`; now part of APILayer |
| needs a key | OpenAQ v3 | 401, `X-API-Key` required |
| needs a key | PatentsView, USPTO ODP, EPO OPS | key or OAuth credentials |
| needs a key | Congress.gov, USDA FoodData Central, NASA | via api.data.gov; `DEMO_KEY` is 30 an hour and 50 a day per IP |
| needs a key | FRED | "All web service requests require an API key" |
| needs a key | GeoNames | 401 without a username |
| needs a key | Europeana, TransportAPI, OpenSanctions, WHO ICD, Japan EDINET | 401 or registration required; TransportAPI free tier 30 a day |
| needs a key | CoinCap | v2 host fails the TLS handshake; v3 answers `Unauthorized` |
| needs a key | ctext.org | `ERR_REQUIRES_AUTHENTICATION`, and the terms forbid automated access |
| paid | Tianyancha, Amap web service, Numbeo | metered, key mandatory, or from USD 260 a month |
| documented as keyed, answers without one | Libraries.io | full JSON without a key against the documented rule; not to be relied on |
| dead | FAA airport status, vizgr historical events, BitcoinAverage, Cryptonator, FreeForexAPI, OSI licence API | connection failure, 404, TLS failure, 522, or 301 into a 404 |
| dead or broken | 天天基金 fundgz, CEIC ajax, Hipolabs universities, Open Elevation | East Money 404 page; expired certificate and 404; connection failure or expired certificate |
| unreachable during the survey, documented as keyless | ECB Data Portal `data-api.ecb.europa.eu`, HKMA API, db.transport.rest, WorldTimeAPI | whole domain 503, 502, 503, or connection closed; retest later; ECB data is mirrored in DBnomics |
| bot wall or header required | NBS `data.stats.gov.cn` (403 WAF), gov.cn policy library search (well-formed JSON, always zero hits), 12306 (302 to an error page), Repology and release-monitoring (reject a generic UA, Anubis challenge), Cloudflare DoH (`Accept: application/dns-json`), Sina quotes and Weibo (Referer), ESPN hidden API (Akamai 403) | as stated |
| works but unofficial | Yahoo Finance, East Money, Tencent quotes, WAQI demo token | Yahoo 429 and crumb checks; WAQI's demo token returns the same Shanghai station for every city |

## GitHub lists worth keeping

| Repository | Stars | Last commit | Notes |
|---|---|---|---|
| public-apis/public-apis | 482,097 | 2026-09-20 | the largest free-API list; a fair share of its `Auth: No` rows now need a key or are dead (REST Countries, exchangerate.host, CoinCap, OpenAQ among those checked), so each row needs a probe |
| awesomedata/awesome-public-datasets | 79,090 | 2026-09-21 | datasets first, a few live APIs; DBnomics came from its Economics section |
| fangzesheng/free-api | 16,261 | 2026-07-25 | the largest Chinese list; many rows point at free-api.com reposts of third-party services, and a good number are metered commercial APIs, so it is a lead list rather than something to wire in directly |
| marcelscruz/public-apis | 9,488 | 2026-09-21 | an active fork of public-apis with a site (publicapis.dev) that filters by auth |
| APIs-guru/openapi-directory | 4,558 | 2026-04-20 | OpenAPI specifications for thousands of public APIs, useful for generating clients and checking parameters |
| jdorfman/awesome-json-datasets | 3,616 | 2026-03-01 (archived) | JSON endpoints without auth; Nobel, data.police.uk and World Bank came from here, but GovTrack, FAA and vizgr rows are dead |
| bytewax/awesome-public-real-time-datasets | 2,925 | 2026-07-10 | real-time streams and APIs, short but maintained |
| NateScarlet/holiday-cn | 2,159 | daily automated | the Chinese public-holiday JSON itself |

No maintained "awesome open data" list for China, the UK, the EU or the US
turned up; the country lists found were for Japan (japan-opendata, 166 stars),
Russia (infoculture, 226) and Switzerland (rnckp, 177).

## Notes for whoever writes the next engines

Headers: Nominatim, MusicBrainz, Open Food Facts, NWS and GitHub require an
identifying User-Agent by policy, and the Nobel, GovTrack and Repology sites
return 403 to a default one, so send the contactable UA (`impersonate = None`
with `api_headers`) as the JSON engines already do. Sina quotes and Weibo need
a Referer; ORCID and Cloudflare DoH need an Accept header.

Formats: Tencent quotes are a GBK-encoded JS variable, the NMC typhoon list is
JSONP, legislation.gov.uk and ORCID default to XML, the Rust channel file is
TOML, the Bank of England and the Fed publish CSV.

Rates to set as engine defaults: Nominatim and MusicBrainz 1 a second, Open
Food Facts 15 a minute, openFDA 240 a minute, BLS v1 25 a day, GLEIF 60 a
minute, W3C 6,000 per 10 minutes, OpenSky 400 credits a day, Comtrade preview
one period and 500 rows per call.

China: the official or near-official sources that answered are holiday-cn,
CFETS central parity, the SAMR standards search, East Money's data centre
(which republishes NBS monthly figures), DBnomics's NBS provider, GLEIF's
Chinese-name search and the Hong Kong Observatory. The NBS site, the gov.cn
policy search, the national law database and 12306 were unusable or
unverified under the survey's conditions; `flk.npc.gov.cn` is a POST interface
and deserves one more try with a real request body.
