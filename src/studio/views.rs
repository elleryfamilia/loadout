//! `maud` server-rendered HTML for load studio: a tabbed shell (Fragments /
//! Profiles), the profile rail + per-fragment detail, the fragment list +
//! modal dialog, the full-width profile editor, and the diff/review.
//!
//! Targets the tiny embedded htmx-shim. Swap targets:
//! - `#main` — the active tab's content (caps list, profile rail, editor, diff).
//! - `#profile-main` — the selected profile's detail (a rail click swaps it in).
//! - `#modal` — the fragment dialog (CSS shows it when non-empty; `/close`
//!   swaps it empty).
//! - `#staged` — the top-bar staged-changes indicator (mutations re-pull it).

use std::path::Path;

use maud::{html, Markup, PreEscaped, DOCTYPE};
use pulldown_cmark::{html as md_html, Event, Options, Parser};

use crate::context::Scope;
use crate::fragment::{Fragment, Layer};
use crate::profile::LoadoutConfig;
use crate::studio::edit::FileDiff;
use crate::studio::state::{
    AtomDot, AtomState, FragmentView, LibraryView, Onboarding, PackView, PreviewCap,
    PreviewOutcome, ProfileView, TargetView, TargetsView, WorkflowSlotView, WorkflowView,
    WorkflowsView,
};
use crate::target::{TargetDef, TargetRule};

/// Script interpreters offered in the fragment dialog.
const SCRIPT_LANGS: &[(&str, &str)] = &[("bash", "Bash"), ("python", "Python"), ("sh", "POSIX sh")];

// --- icons -------------------------------------------------------------------

/// A 16px feather-style inline SVG icon (1.5px stroke, `currentColor`). Matched
/// against a closed set of **static** strings — never interpolate a dynamic
/// value into `PreEscaped` (that would bypass escaping).
fn icon(name: &str) -> Markup {
    let body: &str = match name {
        "plus" => r#"<path d="M12 5v14M5 12h14"/>"#,
        "target" => r#"<circle cx="12" cy="12" r="9"/><circle cx="12" cy="12" r="4"/>"#,
        "sun" => {
            r#"<circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4"/>"#
        }
        "moon" => r#"<path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z"/>"#,
        "monitor" => {
            r#"<rect x="2" y="3" width="20" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>"#
        }
        "copy" => {
            r#"<rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#
        }
        "pencil" => {
            r#"<path d="M12 20h9"/><path d="M16.5 3.5a2.12 2.12 0 0 1 3 3L7 19l-4 1 1-4Z"/>"#
        }
        "trash" => {
            r#"<path d="M3 6h18"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>"#
        }
        "arrow-right" => r#"<path d="M5 12h14"/><path d="m12 5 7 7-7 7"/>"#,
        "arrow-down" => r#"<path d="M12 5v14"/><path d="m19 12-7 7-7-7"/>"#,
        "layers" => {
            r#"<path d="M12 2 2 7l10 5 10-5-10-5Z"/><path d="m2 17 10 5 10-5"/><path d="m2 12 10 5 10-5"/>"#
        }
        "eye" => {
            r#"<path d="M2 12s3.5-7 10-7 10 7 10 7-3.5 7-10 7S2 12 2 12Z"/><circle cx="12" cy="12" r="3"/>"#
        }
        "help" => {
            r#"<circle cx="12" cy="12" r="10"/><path d="M9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3"/><path d="M12 17h.01"/>"#
        }
        "refresh" => {
            r#"<path d="M21 2v6h-6"/><path d="M3 12a9 9 0 0 1 15-6.7L21 8"/><path d="M3 22v-6h6"/><path d="M21 12a9 9 0 0 1-15 6.7L3 16"/>"#
        }
        "check" => r#"<path d="M20 6 9 17l-5-5"/>"#,
        "x" => r#"<path d="M18 6 6 18M6 6l12 12"/>"#,
        "chevron-down" => r#"<path d="m6 9 6 6 6-6"/>"#,
        "power" => r#"<path d="M12 2v10"/><path d="M18.4 6.6a9 9 0 1 1-12.8 0"/>"#,
        "play" => r#"<path d="m6 3 14 9-14 9V3z"/>"#,
        "shield" => r#"<path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z"/>"#,
        "alert" => {
            r#"<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0Z"/><path d="M12 9v4"/><path d="M12 17h.01"/>"#
        }
        "grid" => {
            r#"<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/>"#
        }
        "box" => {
            r#"<path d="M21 8v8a2 2 0 0 1-1 1.73l-7 4a2 2 0 0 1-2 0l-7-4A2 2 0 0 1 3 16V8a2 2 0 0 1 1-1.73l7-4a2 2 0 0 1 2 0l7 4A2 2 0 0 1 21 8Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>"#
        }
        "bolt" => r#"<path d="M13 2 3 14h9l-1 8 10-12h-9l1-8Z"/>"#,
        "terminal" => r#"<path d="m4 17 6-6-6-6"/><path d="M12 19h8"/>"#,
        "code" => r#"<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>"#,
        "git-branch" => {
            r#"<path d="M6 3v12"/><circle cx="18" cy="6" r="3"/><circle cx="6" cy="18" r="3"/><path d="M18 9a9 9 0 0 1-9 9"/>"#
        }
        "database" => {
            r#"<ellipse cx="12" cy="5" rx="9" ry="3"/><path d="M3 5v14a9 3 0 0 0 18 0V5"/><path d="M3 12a9 3 0 0 0 18 0"/>"#
        }
        "server" => {
            r#"<rect x="2" y="3" width="20" height="8" rx="2"/><rect x="2" y="13" width="20" height="8" rx="2"/><path d="M6 7h.01M6 17h.01"/>"#
        }
        "cloud" => r#"<path d="M17.5 19a4.5 4.5 0 1 0 0-9h-1.26A7 7 0 1 0 4 15.25"/>"#,
        "package" => {
            r#"<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/><path d="m7.5 4.3 9 5.1"/>"#
        }
        "wrench" => {
            r#"<path d="M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18v3h3l6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2-2 2.5-2.5z"/>"#
        }
        "flask" => {
            r#"<path d="M9 3h6M10 3v6l-5 9a2 2 0 0 0 1.8 3h10.4a2 2 0 0 0 1.8-3l-5-9V3"/><path d="M7 14h10"/>"#
        }
        "rocket" => {
            r#"<path d="M5 16c-1.5 1.3-2 5-2 5s3.7-.5 5-2c.7-.8.7-2 0-2.8a2 2 0 0 0-3 .8z"/><path d="M12 15l-3-3a16 16 0 0 1 6-10 5 5 0 0 1 7 7 16 16 0 0 1-10 6z"/>"#
        }
        "book" => {
            r#"<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2z"/>"#
        }
        "file" => {
            r#"<path d="M14 3v5h5"/><path d="M14 3H6a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/>"#
        }
        "folder" => {
            r#"<path d="M4 20a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2z"/>"#
        }
        "gear" => {
            r#"<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-2.82 1.17V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 8 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.6 15H4.5a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 6 9.4a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 11 4.6V4.5a2 2 0 0 1 4 0v.09A1.65 1.65 0 0 0 18 6.6l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 11.6"/>"#
        }
        "globe" => {
            r#"<circle cx="12" cy="12" r="9"/><path d="M3 12h18"/><path d="M12 3a14 14 0 0 1 0 18 14 14 0 0 1 0-18z"/>"#
        }
        "cpu" => {
            r#"<rect x="6" y="6" width="12" height="12" rx="1"/><rect x="9" y="9" width="6" height="6"/><path d="M9 2v2M15 2v2M9 20v2M15 20v2M2 9h2M2 15h2M20 9h2M20 15h2"/>"#
        }
        "lock" => {
            r#"<rect x="4" y="11" width="16" height="10" rx="2"/><path d="M8 11V7a4 4 0 0 1 8 0v4"/>"#
        }
        // Built-in target *brand* logos are filled silhouettes, not line art —
        // see `brand_logo` / `brand_svg`, which use a different SVG treatment.
        _ => "",
    };
    PreEscaped(format!(
        r#"<svg class="icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">{body}</svg>"#
    ))
}

// --- target icons ------------------------------------------------------------

/// The built-in language/framework brand logos as filled single-path
/// silhouettes (source: simple-icons, CC0). Java's own mark is Oracle-
/// trademarked, so Java uses the OpenJDK logo. These render *filled*
/// (`brand_svg`), unlike the stroked general-purpose `icon()` glyphs, so they
/// look like the real marks. `name` is a closed-set key and the returned path is
/// a static literal — never interpolated from user input.
fn brand_logo(name: &str) -> Option<&'static str> {
    let d = match name {
        "rust" => {
            r#"M23.8346 11.7033l-1.0073-.6236a13.7268 13.7268 0 00-.0283-.2936l.8656-.8069a.3483.3483 0 00-.1154-.578l-1.1066-.414a8.4958 8.4958 0 00-.087-.2856l.6904-.9587a.3462.3462 0 00-.2257-.5446l-1.1663-.1894a9.3574 9.3574 0 00-.1407-.2622l.49-1.0761a.3437.3437 0 00-.0274-.3361.3486.3486 0 00-.3006-.154l-1.1845.0416a6.7444 6.7444 0 00-.1873-.2268l.2723-1.153a.3472.3472 0 00-.417-.4172l-1.1532.2724a14.0183 14.0183 0 00-.2278-.1873l.0415-1.1845a.3442.3442 0 00-.49-.328l-1.076.491c-.0872-.0476-.1742-.0952-.2623-.1407l-.1903-1.1673A.3483.3483 0 0016.256.955l-.9597.6905a8.4867 8.4867 0 00-.2855-.086l-.414-1.1066a.3483.3483 0 00-.5781-.1154l-.8069.8666a9.2936 9.2936 0 00-.2936-.0284L12.2946.1683a.3462.3462 0 00-.5892 0l-.6236 1.0073a13.7383 13.7383 0 00-.2936.0284L9.9803.3374a.3462.3462 0 00-.578.1154l-.4141 1.1065c-.0962.0274-.1903.0567-.2855.086L7.744.955a.3483.3483 0 00-.5447.2258L7.009 2.348a9.3574 9.3574 0 00-.2622.1407l-1.0762-.491a.3462.3462 0 00-.49.328l.0416 1.1845a7.9826 7.9826 0 00-.2278.1873L3.8413 3.425a.3472.3472 0 00-.4171.4171l.2713 1.1531c-.0628.075-.1255.1509-.1863.2268l-1.1845-.0415a.3462.3462 0 00-.328.49l.491 1.0761a9.167 9.167 0 00-.1407.2622l-1.1662.1894a.3483.3483 0 00-.2258.5446l.6904.9587a13.303 13.303 0 00-.087.2855l-1.1065.414a.3483.3483 0 00-.1155.5781l.8656.807a9.2936 9.2936 0 00-.0283.2935l-1.0073.6236a.3442.3442 0 000 .5892l1.0073.6236c.008.0982.0182.1964.0283.2936l-.8656.8079a.3462.3462 0 00.1155.578l1.1065.4141c.0273.0962.0567.1914.087.2855l-.6904.9587a.3452.3452 0 00.2268.5447l1.1662.1893c.0456.088.0922.1751.1408.2622l-.491 1.0762a.3462.3462 0 00.328.49l1.1834-.0415c.0618.0769.1235.1528.1873.2277l-.2713 1.1541a.3462.3462 0 00.4171.4161l1.153-.2713c.075.0638.151.1255.2279.1863l-.0415 1.1845a.3442.3442 0 00.49.327l1.0761-.49c.087.0486.1741.0951.2622.1407l.1903 1.1662a.3483.3483 0 00.5447.2268l.9587-.6904a9.299 9.299 0 00.2855.087l.414 1.1066a.3452.3452 0 00.5781.1154l.8079-.8656c.0972.0111.1954.0203.2936.0294l.6236 1.0073a.3472.3472 0 00.5892 0l.6236-1.0073c.0982-.0091.1964-.0183.2936-.0294l.8069.8656a.3483.3483 0 00.578-.1154l.4141-1.1066a8.4626 8.4626 0 00.2855-.087l.9587.6904a.3452.3452 0 00.5447-.2268l.1903-1.1662c.088-.0456.1751-.0931.2622-.1407l1.0762.49a.3472.3472 0 00.49-.327l-.0415-1.1845a6.7267 6.7267 0 00.2267-.1863l1.1531.2713a.3472.3472 0 00.4171-.416l-.2713-1.1542c.0628-.0749.1255-.1508.1863-.2278l1.1845.0415a.3442.3442 0 00.328-.49l-.49-1.076c.0475-.0872.0951-.1742.1407-.2623l1.1662-.1893a.3483.3483 0 00.2258-.5447l-.6904-.9587.087-.2855 1.1066-.414a.3462.3462 0 00.1154-.5781l-.8656-.8079c.0101-.0972.0202-.1954.0283-.2936l1.0073-.6236a.3442.3442 0 000-.5892zm-6.7413 8.3551a.7138.7138 0 01.2986-1.396.714.714 0 11-.2997 1.396zm-.3422-2.3142a.649.649 0 00-.7715.5l-.3573 1.6685c-1.1035.501-2.3285.7795-3.6193.7795a8.7368 8.7368 0 01-3.6951-.814l-.3574-1.6684a.648.648 0 00-.7714-.499l-1.473.3158a8.7216 8.7216 0 01-.7613-.898h7.1676c.081 0 .1356-.0141.1356-.088v-2.536c0-.074-.0536-.0881-.1356-.0881h-2.0966v-1.6077h2.2677c.2065 0 1.1065.0587 1.394 1.2088.0901.3533.2875 1.5044.4232 1.8729.1346.413.6833 1.2381 1.2685 1.2381h3.5716a.7492.7492 0 00.1296-.0131 8.7874 8.7874 0 01-.8119.9526zM6.8369 20.024a.714.714 0 11-.2997-1.396.714.714 0 01.2997 1.396zM4.1177 8.9972a.7137.7137 0 11-1.304.5791.7137.7137 0 011.304-.579zm-.8352 1.9813l1.5347-.6824a.65.65 0 00.33-.8585l-.3158-.7147h1.2432v5.6025H3.5669a8.7753 8.7753 0 01-.2834-3.348zm6.7343-.5437V8.7836h2.9601c.153 0 1.0792.1772 1.0792.8697 0 .575-.7107.7815-1.2948.7815zm10.7574 1.4862c0 .2187-.008.4363-.0243.651h-.9c-.09 0-.1265.0586-.1265.1477v.413c0 .973-.5487 1.1846-1.0296 1.2382-.4576.0517-.9648-.1913-1.0275-.4717-.2704-1.5186-.7198-1.8436-1.4305-2.4034.8817-.5599 1.799-1.386 1.799-2.4915 0-1.1936-.819-1.9458-1.3769-2.3153-.7825-.5163-1.6491-.6195-1.883-.6195H5.4682a8.7651 8.7651 0 014.907-2.7699l1.0974 1.151a.648.648 0 00.9182.0213l1.227-1.1743a8.7753 8.7753 0 016.0044 4.2762l-.8403 1.8982a.652.652 0 00.33.8585l1.6178.7188c.0283.2875.0425.577.0425.8717zm-9.3006-9.5993a.7128.7128 0 11.984 1.0316.7137.7137 0 01-.984-1.0316zm8.3389 6.71a.7107.7107 0 01.9395-.3625.7137.7137 0 11-.9405.3635z"#
        }
        "node" => {
            r#"M11.998,24c-0.321,0-0.641-0.084-0.922-0.247l-2.936-1.737c-0.438-0.245-0.224-0.332-0.08-0.383 c0.585-0.203,0.703-0.25,1.328-0.604c0.065-0.037,0.151-0.023,0.218,0.017l2.256,1.339c0.082,0.045,0.197,0.045,0.272,0l8.795-5.076 c0.082-0.047,0.134-0.141,0.134-0.238V6.921c0-0.099-0.053-0.192-0.137-0.242l-8.791-5.072c-0.081-0.047-0.189-0.047-0.271,0 L3.075,6.68C2.99,6.729,2.936,6.825,2.936,6.921v10.15c0,0.097,0.054,0.189,0.139,0.235l2.409,1.392 c1.307,0.654,2.108-0.116,2.108-0.89V7.787c0-0.142,0.114-0.253,0.256-0.253h1.115c0.139,0,0.255,0.112,0.255,0.253v10.021 c0,1.745-0.95,2.745-2.604,2.745c-0.508,0-0.909,0-2.026-0.551L2.28,18.675c-0.57-0.329-0.922-0.945-0.922-1.604V6.921 c0-0.659,0.353-1.275,0.922-1.603l8.795-5.082c0.557-0.315,1.296-0.315,1.848,0l8.794,5.082c0.57,0.329,0.924,0.944,0.924,1.603 v10.15c0,0.659-0.354,1.273-0.924,1.604l-8.794,5.078C12.643,23.916,12.324,24,11.998,24z M19.099,13.993 c0-1.9-1.284-2.406-3.987-2.763c-2.731-0.361-3.009-0.548-3.009-1.187c0-0.528,0.235-1.233,2.258-1.233 c1.807,0,2.473,0.389,2.747,1.607c0.024,0.115,0.129,0.199,0.247,0.199h1.141c0.071,0,0.138-0.031,0.186-0.081 c0.048-0.054,0.074-0.123,0.067-0.196c-0.177-2.098-1.571-3.076-4.388-3.076c-2.508,0-4.004,1.058-4.004,2.833 c0,1.925,1.488,2.457,3.895,2.695c2.88,0.282,3.103,0.703,3.103,1.269c0,0.983-0.789,1.402-2.642,1.402 c-2.327,0-2.839-0.584-3.011-1.742c-0.02-0.124-0.126-0.215-0.253-0.215h-1.137c-0.141,0-0.254,0.112-0.254,0.253 c0,1.482,0.806,3.248,4.655,3.248C17.501,17.007,19.099,15.91,19.099,13.993z"#
        }
        "nextjs" => {
            r#"M18.665 21.978C16.758 23.255 14.465 24 12 24 5.377 24 0 18.623 0 12S5.377 0 12 0s12 5.377 12 12c0 3.583-1.574 6.801-4.067 9.001L9.219 7.2H7.2v9.596h1.615V9.251l9.85 12.727Zm-3.332-8.533 1.6 2.061V7.2h-1.6v6.245Z"#
        }
        "go" => {
            r#"M1.811 10.231c-.047 0-.058-.023-.035-.059l.246-.315c.023-.035.081-.058.128-.058h4.172c.046 0 .058.035.035.07l-.199.303c-.023.036-.082.07-.117.07zM.047 11.306c-.047 0-.059-.023-.035-.058l.245-.316c.023-.035.082-.058.129-.058h5.328c.047 0 .07.035.058.07l-.093.28c-.012.047-.058.07-.105.07zm2.828 1.075c-.047 0-.059-.035-.035-.07l.163-.292c.023-.035.07-.07.117-.07h2.337c.047 0 .07.035.07.082l-.023.28c0 .047-.047.082-.082.082zm12.129-2.36c-.736.187-1.239.327-1.963.514-.176.046-.187.058-.34-.117-.174-.199-.303-.327-.548-.444-.737-.362-1.45-.257-2.115.175-.795.514-1.204 1.274-1.192 2.22.011.935.654 1.706 1.577 1.835.795.105 1.46-.175 1.987-.77.105-.13.198-.27.315-.434H10.47c-.245 0-.304-.152-.222-.35.152-.362.432-.97.596-1.274a.315.315 0 01.292-.187h4.253c-.023.316-.023.631-.07.947a4.983 4.983 0 01-.958 2.29c-.841 1.11-1.94 1.8-3.33 1.986-1.145.152-2.209-.07-3.143-.77-.865-.655-1.356-1.52-1.484-2.595-.152-1.274.222-2.419.993-3.424.83-1.086 1.928-1.776 3.272-2.02 1.098-.2 2.15-.07 3.096.571.62.41 1.063.97 1.356 1.648.07.105.023.164-.117.2m3.868 6.461c-1.064-.024-2.034-.328-2.852-1.029a3.665 3.665 0 01-1.262-2.255c-.21-1.32.152-2.489.947-3.529.853-1.122 1.881-1.706 3.272-1.95 1.192-.21 2.314-.095 3.33.595.923.63 1.496 1.484 1.648 2.605.198 1.578-.257 2.863-1.344 3.962-.771.783-1.718 1.273-2.805 1.495-.315.06-.63.07-.934.106zm2.78-4.72c-.011-.153-.011-.27-.034-.387-.21-1.157-1.274-1.81-2.384-1.554-1.087.245-1.788.935-2.045 2.033-.21.912.234 1.835 1.075 2.21.643.28 1.285.244 1.905-.07.923-.48 1.425-1.228 1.484-2.233z"#
        }
        "python" => {
            r#"M14.25.18l.9.2.73.26.59.3.45.32.34.34.25.34.16.33.1.3.04.26.02.2-.01.13V8.5l-.05.63-.13.55-.21.46-.26.38-.3.31-.33.25-.35.19-.35.14-.33.1-.3.07-.26.04-.21.02H8.77l-.69.05-.59.14-.5.22-.41.27-.33.32-.27.35-.2.36-.15.37-.1.35-.07.32-.04.27-.02.21v3.06H3.17l-.21-.03-.28-.07-.32-.12-.35-.18-.36-.26-.36-.36-.35-.46-.32-.59-.28-.73-.21-.88-.14-1.05-.05-1.23.06-1.22.16-1.04.24-.87.32-.71.36-.57.4-.44.42-.33.42-.24.4-.16.36-.1.32-.05.24-.01h.16l.06.01h8.16v-.83H6.18l-.01-2.75-.02-.37.05-.34.11-.31.17-.28.25-.26.31-.23.38-.2.44-.18.51-.15.58-.12.64-.1.71-.06.77-.04.84-.02 1.27.05zm-6.3 1.98l-.23.33-.08.41.08.41.23.34.33.22.41.09.41-.09.33-.22.23-.34.08-.41-.08-.41-.23-.33-.33-.22-.41-.09-.41.09zm13.09 3.95l.28.06.32.12.35.18.36.27.36.35.35.47.32.59.28.73.21.88.14 1.04.05 1.23-.06 1.23-.16 1.04-.24.86-.32.71-.36.57-.4.45-.42.33-.42.24-.4.16-.36.09-.32.05-.24.02-.16-.01h-8.22v.82h5.84l.01 2.76.02.36-.05.34-.11.31-.17.29-.25.25-.31.24-.38.2-.44.17-.51.15-.58.13-.64.09-.71.07-.77.04-.84.01-1.27-.04-1.07-.14-.9-.2-.73-.25-.59-.3-.45-.33-.34-.34-.25-.34-.16-.33-.1-.3-.04-.25-.02-.2.01-.13v-5.34l.05-.64.13-.54.21-.46.26-.38.3-.32.33-.24.35-.2.35-.14.33-.1.3-.06.26-.04.21-.02.13-.01h5.84l.69-.05.59-.14.5-.21.41-.28.33-.32.27-.35.2-.36.15-.36.1-.35.07-.32.04-.28.02-.21V6.07h2.09l.14.01zm-6.47 14.25l-.23.33-.08.41.08.41.23.33.33.23.41.08.41-.08.33-.23.23-.33.08-.41-.08-.41-.23-.33-.33-.23-.41-.08-.41.08z"#
        }
        "ruby" => {
            r#"M20.156.083c3.033.525 3.893 2.598 3.829 4.77L24 4.822 22.635 22.71 4.89 23.926h.016C3.433 23.864.15 23.729 0 19.139l1.645-3 2.819 6.586.503 1.172 2.805-9.144-.03.007.016-.03 9.255 2.956-1.396-5.431-.99-3.9 8.82-.569-.615-.51L16.5 2.114 20.159.073l-.003.01zM0 19.089zM5.13 5.073c3.561-3.533 8.157-5.621 9.922-3.84 1.762 1.777-.105 6.105-3.673 9.636-3.563 3.532-8.103 5.734-9.864 3.957-1.766-1.777.045-6.217 3.612-9.75l.003-.003z"#
        }
        "php" => {
            r#"M7.01 10.207h-.944l-.515 2.648h.838c.556 0 .97-.105 1.242-.314.272-.21.455-.559.55-1.049.092-.47.05-.802-.124-.995-.175-.193-.523-.29-1.047-.29zM12 5.688C5.373 5.688 0 8.514 0 12s5.373 6.313 12 6.313S24 15.486 24 12c0-3.486-5.373-6.312-12-6.312zm-3.26 7.451c-.261.25-.575.438-.917.551-.336.108-.765.164-1.285.164H5.357l-.327 1.681H3.652l1.23-6.326h2.65c.797 0 1.378.209 1.744.628.366.418.476 1.002.33 1.752a2.836 2.836 0 0 1-.305.847c-.143.255-.33.49-.561.703zm4.024.715l.543-2.799c.063-.318.039-.536-.068-.651-.107-.116-.336-.174-.687-.174H11.46l-.704 3.625H9.388l1.23-6.327h1.367l-.327 1.682h1.218c.767 0 1.295.134 1.586.401s.378.7.263 1.299l-.572 2.944h-1.389zm7.597-2.265a2.782 2.782 0 0 1-.305.847c-.143.255-.33.49-.561.703a2.44 2.44 0 0 1-.917.551c-.336.108-.765.164-1.286.164h-1.18l-.327 1.682h-1.378l1.23-6.326h2.649c.797 0 1.378.209 1.744.628.366.417.477 1.001.331 1.751zM17.766 10.207h-.943l-.516 2.648h.838c.557 0 .971-.105 1.242-.314.272-.21.455-.559.551-1.049.092-.47.049-.802-.125-.995s-.524-.29-1.047-.29z"#
        }
        "swift" => {
            r#"M7.508 0c-.287 0-.573 0-.86.002-.241.002-.483.003-.724.01-.132.003-.263.009-.395.015A9.154 9.154 0 0 0 4.348.15 5.492 5.492 0 0 0 2.85.645 5.04 5.04 0 0 0 .645 2.848c-.245.48-.4.972-.495 1.5-.093.52-.122 1.05-.136 1.576a35.2 35.2 0 0 0-.012.724C0 6.935 0 7.221 0 7.508v8.984c0 .287 0 .575.002.862.002.24.005.481.012.722.014.526.043 1.057.136 1.576.095.528.25 1.02.495 1.5a5.03 5.03 0 0 0 2.205 2.203c.48.244.97.4 1.498.495.52.093 1.05.124 1.576.138.241.007.483.009.724.01.287.002.573.002.86.002h8.984c.287 0 .573 0 .86-.002.241-.001.483-.003.724-.01a10.523 10.523 0 0 0 1.578-.138 5.322 5.322 0 0 0 1.498-.495 5.035 5.035 0 0 0 2.203-2.203c.245-.48.4-.972.495-1.5.093-.52.124-1.05.138-1.576.007-.241.009-.481.01-.722.002-.287.002-.575.002-.862V7.508c0-.287 0-.573-.002-.86a33.662 33.662 0 0 0-.01-.724 10.5 10.5 0 0 0-.138-1.576 5.328 5.328 0 0 0-.495-1.5A5.039 5.039 0 0 0 21.152.645 5.32 5.32 0 0 0 19.654.15a10.493 10.493 0 0 0-1.578-.138 34.98 34.98 0 0 0-.722-.01C17.067 0 16.779 0 16.492 0H7.508zm6.035 3.41c4.114 2.47 6.545 7.162 5.549 11.131-.024.093-.05.181-.076.272l.002.001c2.062 2.538 1.5 5.258 1.236 4.745-1.072-2.086-3.066-1.568-4.088-1.043a6.803 6.803 0 0 1-.281.158l-.02.012-.002.002c-2.115 1.123-4.957 1.205-7.812-.022a12.568 12.568 0 0 1-5.64-4.838c.649.48 1.35.902 2.097 1.252 3.019 1.414 6.051 1.311 8.197-.002C9.651 12.73 7.101 9.67 5.146 7.191a10.628 10.628 0 0 1-1.005-1.384c2.34 2.142 6.038 4.83 7.365 5.576C8.69 8.408 6.208 4.743 6.324 4.86c4.436 4.47 8.528 6.996 8.528 6.996.154.085.27.154.36.213.085-.215.16-.437.224-.668.708-2.588-.09-5.548-1.893-7.992z"#
        }
        "dotnet" => {
            r#"M24 8.77h-2.468v7.565h-1.425V8.77h-2.462V7.53H24zm-6.852 7.565h-4.821V7.53h4.63v1.24h-3.205v2.494h2.953v1.234h-2.953v2.604h3.396zm-6.708 0H8.882L4.78 9.863a2.896 2.896 0 0 1-.258-.51h-.036c.032.189.048.592.048 1.21v5.772H3.157V7.53h1.659l3.965 6.32c.167.261.275.442.323.54h.024c-.04-.233-.06-.629-.06-1.185V7.529h1.372zm-8.703-.693a.868.829 0 0 1-.869.829.868.829 0 0 1-.868-.83.868.829 0 0 1 .868-.828.868.829 0 0 1 .869.829Z"#
        }
        "bun" => {
            r#"M12 22.596c6.628 0 12-4.338 12-9.688 0-3.318-2.057-6.248-5.219-7.986-1.286-.715-2.297-1.357-3.139-1.89C14.058 2.025 13.08 1.404 12 1.404c-1.097 0-2.334.785-3.966 1.821a49.92 49.92 0 0 1-2.816 1.697C2.057 6.66 0 9.59 0 12.908c0 5.35 5.372 9.687 12 9.687v.001ZM10.599 4.715c.334-.759.503-1.58.498-2.409 0-.145.202-.187.23-.029.658 2.783-.902 4.162-2.057 4.624-.124.048-.199-.121-.103-.209a5.763 5.763 0 0 0 1.432-1.977Zm2.058-.102a5.82 5.82 0 0 0-.782-2.306v-.016c-.069-.123.086-.263.185-.172 1.962 2.111 1.307 4.067.556 5.051-.082.103-.23-.003-.189-.126a5.85 5.85 0 0 0 .23-2.431Zm1.776-.561a5.727 5.727 0 0 0-1.612-1.806v-.014c-.112-.085-.024-.274.114-.218 2.595 1.087 2.774 3.18 2.459 4.407a.116.116 0 0 1-.049.071.11.11 0 0 1-.153-.026.122.122 0 0 1-.022-.083 5.891 5.891 0 0 0-.737-2.331Zm-5.087.561c-.617.546-1.282.76-2.063 1-.117 0-.195-.078-.156-.181 1.752-.909 2.376-1.649 2.999-2.778 0 0 .155-.118.188.085 0 .304-.349 1.329-.968 1.874Zm4.945 11.237a2.957 2.957 0 0 1-.937 1.553c-.346.346-.8.565-1.286.62a2.178 2.178 0 0 1-1.327-.62 2.955 2.955 0 0 1-.925-1.553.244.244 0 0 1 .064-.198.234.234 0 0 1 .193-.069h3.965a.226.226 0 0 1 .19.07c.05.053.073.125.063.197Zm-5.458-2.176a1.862 1.862 0 0 1-2.384-.245 1.98 1.98 0 0 1-.233-2.447c.207-.319.503-.566.848-.713a1.84 1.84 0 0 1 1.092-.11c.366.075.703.261.967.531a1.98 1.98 0 0 1 .408 2.114 1.931 1.931 0 0 1-.698.869v.001Zm8.495.005a1.86 1.86 0 0 1-2.381-.253 1.964 1.964 0 0 1-.547-1.366c0-.384.11-.76.32-1.079.207-.319.503-.567.849-.713a1.844 1.844 0 0 1 1.093-.108c.367.076.704.262.968.534a1.98 1.98 0 0 1 .4 2.117 1.932 1.932 0 0 1-.702.868Z"#
        }
        "java" => {
            r#"M11.915 0 11.7.215C9.515 2.4 7.47 6.39 6.046 10.483c-1.064 1.024-3.633 2.81-3.711 3.551-.093.87 1.746 2.611 1.55 3.235-.198.625-1.304 1.408-1.014 1.939.1.188.823.011 1.277-.491a13.389 13.389 0 0 0-.017 2.14c.076.906.27 1.668.643 2.232.372.563.956.911 1.667.911.397 0 .727-.114 1.024-.264.298-.149.571-.33.91-.5.68-.34 1.634-.666 3.53-.604 1.903.062 2.872.39 3.559.704.687.314 1.15.664 1.925.664.767 0 1.395-.336 1.807-.9.412-.563.631-1.33.72-2.24.06-.623.055-1.32 0-2.066.454.45 1.117.604 1.213.424.29-.53-.816-1.314-1.013-1.937-.198-.624 1.642-2.366 1.549-3.236-.08-.748-2.707-2.568-3.748-3.586C16.428 6.374 14.308 2.394 12.13.215zm.175 6.038a2.95 2.95 0 0 1 2.943 2.942 2.95 2.95 0 0 1-2.943 2.943A2.95 2.95 0 0 1 9.148 8.98a2.95 2.95 0 0 1 2.942-2.942zM8.685 7.983a3.515 3.515 0 0 0-.145.997c0 1.951 1.6 3.55 3.55 3.55 1.95 0 3.55-1.598 3.55-3.55 0-.329-.046-.648-.132-.951.334.095.64.208.915.336a42.699 42.699 0 0 1 2.042 5.829c.678 2.545 1.01 4.92.846 6.607-.082.844-.29 1.51-.606 1.94-.315.431-.713.651-1.315.651-.593 0-.932-.27-1.673-.61-.741-.338-1.825-.694-3.792-.758-1.974-.064-3.073.293-3.821.669-.375.188-.659.373-.911.5s-.466.2-.752.2c-.53 0-.876-.209-1.16-.64-.285-.43-.474-1.101-.545-1.948-.141-1.693.176-4.069.823-6.614a43.155 43.155 0 0 1 1.934-5.783c.348-.167.749-.31 1.192-.425zm-3.382 4.362a.216.216 0 0 1 .13.031c-.166.56-.323 1.116-.463 1.665a33.849 33.849 0 0 0-.547 2.555 3.9 3.9 0 0 0-.2-.39c-.58-1.012-.914-1.642-1.16-2.08.315-.24 1.679-1.755 2.24-1.781zm13.394.01c.562.027 1.926 1.543 2.24 1.783-.246.438-.58 1.068-1.16 2.08a4.428 4.428 0 0 0-.163.309 32.354 32.354 0 0 0-.562-2.49 40.579 40.579 0 0 0-.482-1.652.216.216 0 0 1 .127-.03z"#
        } // OpenJDK (Java's logo is Oracle-trademarked; OpenJDK is the CC0 mark)
        _ => return None,
    };
    Some(d)
}

/// Brand-logo target icons offered in the picker (and the built-in defaults), in
/// display order. Keep in sync with `brand_logo` and `target::builtin_icon`.
const BRAND_ICONS: &[&str] = &[
    "rust", "node", "bun", "nextjs", "go", "python", "java", "ruby", "php", "swift", "dotnet",
];

/// General-purpose line glyphs (from `icon()`) a custom target may pick when no
/// brand logo fits. A name in neither this set nor `brand_logo` renders as a
/// lettermark badge.
const GENERIC_GLYPHS: &[&str] = &[
    "box",
    "package",
    "layers",
    "terminal",
    "code",
    "git-branch",
    "database",
    "server",
    "cloud",
    "globe",
    "cpu",
    "gear",
    "wrench",
    "flask",
    "rocket",
    "book",
    "file",
    "folder",
    "bolt",
    "monitor",
    "lock",
    "target",
];

/// Whether `name` is a general-purpose line glyph a target chip can render.
/// Returns the matching `&'static str` so callers get a static name for `icon()`.
fn target_glyph(name: &str) -> Option<&'static str> {
    GENERIC_GLYPHS.iter().copied().find(|&g| g == name)
}

/// A target's resolved chip mark: a filled brand logo, a line glyph, or a short
/// uppercase lettermark badge (the fallback for a custom target with no fitting
/// icon).
enum TargetIcon {
    Logo(&'static str),
    Glyph(&'static str),
    Mark(String),
}

/// Resolve a target's icon for rendering. `token` is the stored icon name (a
/// built-in's brand, a custom target's chosen icon, or `None`); `id` is the
/// target id used to derive the lettermark fallback. A token naming a brand logo
/// renders the filled logo; one naming a general glyph renders that line glyph; a
/// non-empty unknown token marks from its own text (so `icon = "k8s"` shows
/// `K8`); an absent/empty token marks from the id.
fn resolve_target_icon(token: Option<&str>, id: &str) -> TargetIcon {
    match token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(name) => {
            if let Some(p) = brand_logo(name) {
                TargetIcon::Logo(p)
            } else if let Some(g) = target_glyph(name) {
                TargetIcon::Glyph(g)
            } else {
                TargetIcon::Mark(crate::target::lettermark(name))
            }
        }
        None => TargetIcon::Mark(crate::target::lettermark(id)),
    }
}

/// Render a filled brand-logo SVG. `path` comes only from `brand_logo` (a closed
/// set of static literals), never from user input, so PreEscaping it is safe.
fn brand_svg(path: &str) -> Markup {
    PreEscaped(format!(
        r#"<svg class="icon brand" viewBox="0 0 24 24" fill="currentColor" aria-hidden="true"><path d="{path}"/></svg>"#
    ))
}

/// Render a resolved target icon: a filled brand logo, a line glyph, or a
/// lettermark badge `span`.
fn target_icon_markup(ti: &TargetIcon) -> Markup {
    match ti {
        TargetIcon::Logo(path) => brand_svg(path),
        TargetIcon::Glyph(name) => icon(name),
        TargetIcon::Mark(letters) => html! { span class="lettermark" { (letters) } },
    }
}

/// A single target chip: the resolved icon + the id label. `token` is the
/// target's stored icon name (built-in brand or custom pick), or `None` to derive
/// a lettermark from the id.
fn target_chip(id: &str, token: Option<&str>) -> Markup {
    let ti = resolve_target_icon(token, id);
    html! { span class="target-chip" { (target_icon_markup(&ti)) (id) } }
}

/// A target's icon with no label — for the profile card's top-right cluster,
/// where the id is dropped. The id is kept as a `title` for hover/discovery.
fn target_icon_only(id: &str, token: Option<&str>) -> Markup {
    let ti = resolve_target_icon(token, id);
    html! { span class="rail-icon" title=(id) { (target_icon_markup(&ti)) } }
}

/// The loadout brandmark: a backpack — the gear you equip before a job.
/// Bootstrap Icons `backpack2` (MIT). Filled `currentColor`, so the CSS sets
/// its accent color.
fn brand_mark() -> Markup {
    PreEscaped(
        r##"<svg class="mark" viewBox="0 0 16 16" fill="currentColor" aria-hidden="true">
  <path d="M4.04 7.43a4 4 0 0 1 7.92 0 .5.5 0 1 1-.99.14 3 3 0 0 0-5.94 0 .5.5 0 1 1-.99-.14"/>
  <path fill-rule="evenodd" d="M4 9.5a.5.5 0 0 1 .5-.5h7a.5.5 0 0 1 .5.5v4a.5.5 0 0 1-.5.5h-7a.5.5 0 0 1-.5-.5zm1 .5v3h6v-3h-1v.5a.5.5 0 0 1-1 0V10z"/>
  <path d="M6 2.341V2a2 2 0 1 1 4 0v.341c2.33.824 4 3.047 4 5.659v1.191l1.17.585a1.5 1.5 0 0 1 .83 1.342V13.5a1.5 1.5 0 0 1-1.5 1.5h-1c-.456.607-1.182 1-2 1h-7a2.5 2.5 0 0 1-2-1h-1A1.5 1.5 0 0 1 0 13.5v-2.382a1.5 1.5 0 0 1 .83-1.342L2 9.191V8a6 6 0 0 1 4-5.659M7 2v.083a6 6 0 0 1 2 0V2a1 1 0 0 0-2 0M3 13.5A1.5 1.5 0 0 0 4.5 15h7a1.5 1.5 0 0 0 1.5-1.5V8A5 5 0 0 0 3 8zm-1-3.19-.724.362a.5.5 0 0 0-.276.447V13.5a.5.5 0 0 0 .5.5H2zm12 0V14h.5a.5.5 0 0 0 .5-.5v-2.382a.5.5 0 0 0-.276-.447L14 10.309Z"/>
</svg>"##
            .to_string(),
    )
}

/// Inline `<head>` script that resolves the stored theme preference
/// (`auto`/`light`/`dark`) against the system `prefers-color-scheme` and stamps
/// `<html data-theme>` (resolved) + `<html data-theme-pref>` (preference) before
/// the stylesheet paints — preventing a dark→light flash on load.
const THEME_INIT_JS: &str = "(function(){try{var p=localStorage.getItem('loadout-theme')||'auto';\
var m=window.matchMedia&&matchMedia('(prefers-color-scheme: light)').matches;\
var e=p==='auto'?(m?'light':'dark'):p;var r=document.documentElement;\
r.dataset.theme=e;r.dataset.themePref=p;}catch(_){}})();";

/// The theme toggle: one button cycling auto → light → dark. All three glyphs are
/// rendered; CSS shows the one matching `<html data-theme-pref>`, and `studio.js`
/// flips the preference + persists it on click. Defaults to the auto (monitor)
/// glyph until JS/the inline init sets the preference.
fn theme_toggle() -> Markup {
    html! {
        button id="theme-toggle" type="button" class="icon-btn theme-toggle"
            title="Theme: auto" aria-label="Switch color theme" {
            span class="ti ti-auto" { (icon("monitor")) }
            span class="ti ti-light" { (icon("sun")) }
            span class="ti ti-dark" { (icon("moon")) }
        }
    }
}

/// The glyph for a fragment row, derived from its content type.
fn fragment_icon_name(c: &FragmentView) -> &'static str {
    crate::studio::state::type_glyph(c.kind, c.script_lang.as_deref())
}

// --- markdown ----------------------------------------------------------------

/// Render overlay markdown to HTML. Raw HTML is escaped (studio can open an
/// untrusted cloned repo's guidance) and generated header comments are stripped.
fn render_markdown(md: &str) -> Markup {
    let body = strip_leading_comments(md);
    let opts = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(body, opts).map(|ev| match ev {
        Event::Html(s) | Event::InlineHtml(s) => Event::Text(s),
        other => other,
    });
    let mut out = String::new();
    md_html::push_html(&mut out, parser);
    PreEscaped(out)
}

fn strip_leading_comments(md: &str) -> &str {
    let mut t = md.trim_start();
    while let Some(rest) = t.strip_prefix("<!--") {
        match rest.find("-->") {
            Some(end) => t = rest[end + 3..].trim_start(),
            None => break,
        }
    }
    t
}

// --- page shell --------------------------------------------------------------

/// The full page: top bar (brand + tabs + staged indicator), the `#main` tab
/// content, and the empty `#modal` container.
pub fn shell(main: Markup, staged: usize, active_tab: &str) -> String {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                // No-flash theme init: resolve the stored preference (auto/light/
                // dark) against the system setting and stamp <html> before the
                // stylesheet paints, so there's no dark→light flicker on load.
                script { (PreEscaped(THEME_INIT_JS)) }
                title { "Loadout studio" }
                link rel="stylesheet" href="/assets/studio.css";
                script src="/assets/studio.js" defer {}
            }
            body {
                header class="topbar" {
                    div class="brand" { span class="brand-mark" { (brand_mark()) } span class="brand-name" { "Loadout" } }
                    (tab_bar(active_tab))
                    div class="topbar-right" {
                        div id="staged" class="staged-wrap" { (staged_indicator(staged)) }
                        button type="button" class="icon-btn" title="Show me around" hx-get="/onboarding/welcome" hx-target="#main" { (icon("help")) }
                        (theme_toggle())
                    }
                }
                main class="main" id="main" { (main) }
                div id="modal" class="modal-root" {}
            }
        }
    }
    .into_string()
}

fn tab_bar(active: &str) -> Markup {
    let cls = |name: &str| if name == active { "tab active" } else { "tab" };
    html! {
        nav class="tabs" {
            button class=(cls("profiles")) data-tab="profiles" hx-get="/tab/profiles" hx-target="#main" { (icon("layers")) "Loadouts" }
            button class=(cls("fragments")) data-tab="fragments" hx-get="/tab/fragments" hx-target="#main" { (icon("box")) "Fragments" }
            button class=(cls("targets")) data-tab="targets" hx-get="/tab/targets" hx-target="#main" { (icon("target")) "Targets" }
            button class=(cls("workflows")) data-tab="workflows" hx-get="/tab/workflows" hx-target="#main" { (icon("git-branch")) "Workflows" }
        }
    }
}

/// The staged-changes indicator (top-bar right). Re-pulled via `GET /staged`.
pub fn staged_indicator(staged: usize) -> Markup {
    html! {
        @if staged > 0 {
            span class="staged-count" { (icon("layers")) (staged) " staged" }
            button class="btn btn-ghost btn-sm" hx-get="/diff" hx-target="#main" { "Review" }
            button class="btn btn-ghost btn-sm" hx-post="/discard" hx-target="#main"
                hx-confirm="Discard all staged changes? Your config files won't be modified." { (icon("x")) "Discard" }
            button class="btn btn-primary btn-sm" hx-post="/apply" hx-target="#main"
                hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply" }
        } @else {
            span class="muted small" { "No staged changes" }
        }
    }
}

pub fn staged_indicator_fragment(staged: usize) -> String {
    staged_indicator(staged).into_string()
}

/// A one-shot loader that re-pulls the staged indicator after a mutation.
fn staged_refresh() -> Markup {
    html! { div hx-get="/staged" hx-trigger="load" hx-target="#staged" {} }
}

/// The staged-indicator refresh loader as a standalone string (for handlers that
/// append it to a non-fragment response, e.g. the inline fragment-add editor reload).
pub fn staged_indicator_loader() -> String {
    staged_refresh().into_string()
}

/// A one-shot loader that closes the modal after a fragment mutation.
fn modal_close() -> Markup {
    html! { div hx-get="/close" hx-trigger="load" hx-target="#modal" {} }
}

/// The modal-close loader as a standalone string (appended to a profile-detail
/// re-render when a fragment was edited from inside a profile).
pub fn modal_close_loader() -> String {
    modal_close().into_string()
}

// --- Profiles tab ------------------------------------------------------------

/// The selected profile's rendered preview, bundled for the detail view.
pub struct ProfileDetail<'a> {
    pub name: &'a str,
    pub outcome: &'a PreviewOutcome,
    pub disabled: bool,
    /// Which fragment card(s) to render expanded. Cards collapse by default; a
    /// fragment that was just run opens so its fresh output is visible.
    pub expand: Expand<'a>,
    /// `(fragment_id, message)` when a just-run command **failed** — that card's
    /// body shows the error and a retry button instead of (blank) output.
    pub failed: Option<(String, String)>,
}

/// Which fragment cards open after an action. Passive views ([`Expand::None`])
/// collapse everything; running one fragment opens that one ([`Expand::One`]);
/// "Run all" opens every dynamic card ([`Expand::AllDynamic`]).
#[derive(Clone, Copy)]
pub enum Expand<'a> {
    None,
    One(&'a str),
    AllDynamic,
}

/// The Profiles tab: a vertical profile rail (left) + the selected profile's
/// detail (right). `selected` is the profile whose detail fills the main area
/// (the server default-selects the bound/first profile so it's never empty).
pub fn profiles_tab(
    lib: &LibraryView,
    selected: Option<ProfileDetail>,
    flash: Option<&str>,
    onboarding: Option<&Onboarding>,
    packs: &[PackView],
) -> Markup {
    let sel_name = selected.as_ref().map(|d| d.name);
    html! {
        div class="tab-profiles" {
            aside class="profile-rail" {
                div class="rail-head" {
                    div class="rail-head-actions" {
                        button class="btn btn-ghost btn-sm" hx-get="/packs" hx-target="#main" { (icon("grid")) "Starter packs" }
                        button class="btn btn-primary btn-sm" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "New" }
                    }
                }
                @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
                @if lib.profiles.is_empty() {
                    p class="rail-empty muted" { "No loadouts yet." }
                } @else {
                    nav class="rail-list" {
                        @for p in &lib.profiles { (profile_rail_item(p, sel_name == Some(p.name.as_str()))) }
                    }
                }
            }
            section class="profile-main" id="profile-main" {
                @match &selected {
                    Some(detail) => (profile_detail(detail)),
                    None => {
                        @if lib.profiles.is_empty() {
                            @match onboarding {
                                Some(o) => (studio_welcome(o, packs)),
                                None => (profiles_empty_main()),
                            }
                        } @else { (profile_pick_prompt()) }
                    }
                }
            }
        }
    }
}

/// First-launch welcome shown on the Profiles tab when the config is fresh (no
/// profiles and no own fragments): confirm what was detected, explain what a
/// profile is for (and why the overlay is empty), then offer the starter-pack
/// gallery (recommended pack first) or a from-scratch composer.
fn studio_welcome(o: &Onboarding, packs: &[PackView]) -> Markup {
    let scope_label = match o.scope {
        Scope::Repo => "repo",
        Scope::Machine => "machine",
    };
    html! {
        div class="welcome" {
            div class="welcome-head" {
                span class="welcome-wave" { "👋" }
                h1 { "Welcome to Loadout studio" }
            }
            div class="welcome-detect" {
                span class="muted small" { "loadout detected" }
                span class="welcome-chips" {
                    @match &o.stack {
                        Some(s) => (target_chip(s, crate::target::builtin_icon(s))),
                        None => span class="target-chip muted" { "no specific stack" },
                    }
                    span class="target-chip" { (scope_label) }
                    @if let Some(b) = &o.branch { span class="target-chip muted" { "branch " (b) } }
                }
            }
            p class="welcome-lead" { "A " strong { "loadout" } " decides what guidance your agent gets here. Apply a " strong { "starter pack" } " to get one in a click." }
            p class="muted" { "Each pack copies a curated set of fragments into your library and creates a ready-made loadout — all staged; nothing is saved until you Apply. You can customize everything afterward." }
            (legend())
            div class="pack-grid" { @for p in packs { (pack_card(p)) } }
            div class="welcome-actions" {
                button class="btn btn-ghost" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "Start from scratch" }
            }
            // The skill card loads lazily so the welcome render never blocks on
            // (or threads through) global-filesystem state.
            div id="skill-card" hx-get="/skills/card" hx-trigger="load" hx-target="#skill-card" {}
        }
    }
}

// --- agent skill card -------------------------------------------------------

/// What the skill card shows; derived from the real filesystem by the server
/// (never from session state — installing is a direct action, not a staged op).
pub enum SkillCardState {
    /// Not installed: offer the install button.
    Offer,
    /// Installed and current: show the handoff command.
    Installed,
    /// Installed but this loadout ships a newer version.
    UpgradeAvailable,
    /// Present with local edits (or a copy loadout didn't write) — hands off.
    HandsOff,
}

/// The agent-skill card (fills `#skill-card`). `ids` lists the shipped skills
/// (display only). Unlike packs, the Install button writes `~/.agents/skills`
/// immediately on confirm — there is nothing staged to review or discard, so
/// it must not imply staged semantics.
pub fn skill_card(ids: &[&str], state: &SkillCardState) -> String {
    let id_list = ids.join(", ");
    html! {
        div class="cmd-block" {
            @match state {
                SkillCardState::Offer => {
                    span class="muted small" {
                        "loadout ships agent skills (" strong { (id_list) } "): import an existing "
                        "CLAUDE.md/AGENTS.md, and save preferences you state mid-session as global guidance "
                        "(work in Claude Code, Codex, Gemini CLI, opencode)."
                    }
                    button class="btn btn-ghost"
                        hx-post="/skills/install" hx-target="#skill-card"
                        hx-confirm=(format!(
                            "Install the loadout skills ({id_list}) into ~/.agents/skills now? \
                             This writes files immediately (not staged); `load skill remove` undoes it."
                        )) {
                        (icon("bolt")) "Install the skills"
                    }
                }
                SkillCardState::Installed => {
                    span class="muted small" { "The loadout skills (" strong { (id_list) } ") are installed. Import your existing instructions from any agent session:" }
                    code { "load run claude -- \"/loadout-migrate\"" }
                    span class="muted small" { "remove with " code { "load skill remove" } }
                }
                SkillCardState::UpgradeAvailable => {
                    span class="muted small" { "The loadout skills (" strong { (id_list) } ") are installed but a newer version ships with this loadout." }
                    button class="btn btn-ghost"
                        hx-post="/skills/install" hx-target="#skill-card"
                        hx-confirm="Upgrade the loadout skills in ~/.agents/skills? This rewrites the skill files immediately." {
                        (icon("refresh")) "Upgrade the skills"
                    }
                }
                SkillCardState::HandsOff => {
                    span class="muted small" {
                        "Skills (" strong { (id_list) } ") exist in ~/.agents/skills with local edits — loadout leaves them alone."
                    }
                }
            }
        }
    }
    .into_string()
}

/// The welcome as a standalone `#main` fragment — the "?" tour button, reachable
/// any time. Unlike the first-launch welcome (which lives inside the Profiles
/// tab), this takes over `#main` while the nav still highlights the prior tab, so
/// it's wrapped in a dimmed overlay with a "Quick tour" bar + Close to read
/// clearly as a separate screen rather than that tab's content.
pub fn welcome_fragment(o: &Onboarding, packs: &[PackView]) -> String {
    html! {
        div class="welcome-overlay" {
            div class="welcome-tourbar" {
                span class="welcome-tourtag" { (icon("help")) "Quick tour" }
                button class="btn btn-ghost btn-sm" hx-get="/tab/profiles" hx-target="#main" { (icon("x")) "Close" }
            }
            (studio_welcome(o, packs))
        }
    }
    .into_string()
}

/// One row in the profile rail: name + status + targets + fragment dots.
/// Selecting it swaps the detail into `#profile-main`.
fn profile_rail_item(p: &ProfileView, active: bool) -> Markup {
    let name = p.name.as_str();
    let e = enc(name);
    let mut cls = String::from("rail-item");
    if active {
        cls.push_str(" active");
    }
    if p.disabled {
        cls.push_str(" disabled");
    }
    html! {
        div class=(cls) role="button" tabindex="0" data-profile=(name)
            hx-get=(format!("/profiles/{e}/select")) hx-target="#profile-main" {
            span class="rail-top" {
                span class="rail-name" { (name) }
                @if p.disabled { span class="tag off-tag" { "off" } }
                // Target icons (no labels) cluster at the card's top-right.
                @if !p.targets.is_empty() {
                    span class="rail-icons" { @for t in &p.targets { (target_icon_only(&t.id, t.icon.as_deref())) } }
                }
            }
            span class="rail-foot" {
                @if p.atoms.is_empty() {
                    span class="muted small" { "no fragments" }
                } @else {
                    span class="atoms" { @for a in &p.atoms { (atom_dot(a)) } }
                    span class="muted small" { (p.atoms.len()) }
                }
            }
        }
    }
}

/// The selected profile's detail (fills `#profile-main`): a header with the
/// profile name + actions, a provenance breadcrumb, then one expandable card
/// per composed fragment.
pub fn profile_detail(d: &ProfileDetail) -> Markup {
    let p = d.outcome;
    let name = d.name;
    let e = enc(name);
    let n = p.caps.len();
    html! {
        div class="detail" {
            div class="detail-head" {
                div class="detail-title" {
                    h1 { (name) }
                    @if d.disabled { span class="tag off-tag" { "disabled" } }
                }
                div class="detail-actions" {
                    @if !p.agent.is_empty() { span class="chip chip-agent" title="rendered for this agent" { (p.agent.as_str()) } }
                    button class="toggle" title=(if d.disabled { "Enable loadout" } else { "Disable loadout" }) aria-label="Toggle loadout"
                        hx-post=(format!("/profiles/{e}/disable")) hx-target="#main" {
                        span class=(if d.disabled { "switch off" } else { "switch on" }) {}
                    }
                    button class="icon-btn" title="Edit" aria-label=(format!("Edit {name}"))
                        hx-get=(format!("/profiles/{e}/edit")) hx-target="#main" { (icon("pencil")) }
                    button class="icon-btn danger" title="Delete" aria-label=(format!("Delete {name}"))
                        hx-delete=(format!("/profiles/{e}")) hx-target="#main"
                        hx-confirm=(format!("Stage deletion of loadout \"{name}\"?")) { (icon("trash")) }
                }
            }
            div class="provenance" {
                span class="prov-node" { (p.context_summary.as_str()) }
                span class="prov-arrow" { (icon("arrow-right")) }
                span class="prov-node" { (n) " " (if n == 1 { "fragment" } else { "fragments" }) }
                @if p.caps.iter().any(|c| c.dynamic) {
                    span class="prov-spacer" {}
                    button type="button" class="btn btn-ghost btn-sm run-all"
                        title="Run every script/provider in this loadout and show the live output it adds"
                        hx-post=(format!("/profiles/{e}/run")) hx-target="#profile-main" {
                        (icon("play")) "Run all scripts"
                    }
                }
            }
            @if let Some(note) = &p.note { p class="note" { (note) } }
            @if p.caps.is_empty() {
                div class="detail-blank" {
                    (icon("eye"))
                    p class="muted" { "This loadout composes no guidance for " (p.agent.as_str()) " in this context." }
                }
            } @else {
                div class="detail-doc" { @for c in &p.caps {
                    (preview_fragment_card(c, name, d.expand, failed_msg(&d.failed, &c.id)))
                } }
            }
        }
    }
}

pub fn profile_detail_fragment(d: &ProfileDetail) -> String {
    profile_detail(d).into_string()
}

/// One collapsible fragment section inside the profile "document": a compact
/// summary row that, when opened, reveals the fragment's rendered-markdown
/// guidance (the prominent content) plus an "Edit fragment" action.
/// The failure message for fragment `id`, if this render carries one — matched
/// by id so only the card that actually failed shows the error + retry.
fn failed_msg<'a>(failed: &'a Option<(String, String)>, id: &str) -> Option<&'a str> {
    failed
        .as_ref()
        .filter(|(fid, _)| fid == id)
        .map(|(_, msg)| msg.as_str())
}

fn preview_fragment_card(
    c: &PreviewCap,
    profile: &str,
    expand: Expand,
    failed: Option<&str>,
) -> Markup {
    let glyph = c.glyph;
    // Cards start collapsed on a passive view (the user opens what they care
    // about), but a just-run fragment stays open so its fresh output is visible.
    // `has_output`/`prompt` pick what the body shows once expanded: live output
    // for a dynamic cap that ran, or a centered "Run" prompt for one that hasn't.
    // A dynamic cap can also be run from the summary's corner button. A failed
    // run takes over the body with an error + retry, regardless of the above.
    let has_output = c.dynamic && !c.pending && !c.skipped && failed.is_none();
    let prompt = c.dynamic && c.pending && failed.is_none();
    // A failed card opens so its error is visible even on a passive re-render.
    let open = failed.is_some()
        || match expand {
            Expand::None => false,
            Expand::One(id) => c.id == id,
            Expand::AllDynamic => c.dynamic,
        };
    let run_url = format!("/fragments/{}/run?profile={}", enc(&c.id), enc(profile));
    html! {
        details class="fragment-detail" open[open] {
            summary class="fragment-detail-head" {
                span class=(format!("fragment-glyph k-{}", c.kind)) { (icon(glyph)) }
                span class="fragment-detail-title" { (c.title) }
                span class="fragment-detail-id" { (c.id) }
                span class="fragment-detail-spacer" {}
                @if c.dynamic {
                    button type="button" class="btn btn-ghost btn-xs fragment-run"
                        title="Run this script now and show its output"
                        hx-post=(run_url.clone())
                        hx-target="#profile-main" {
                        (icon("play")) (if c.pending { "Run" } else { "Re-run" })
                    }
                }
                @if c.skipped { span class="tag off-tag" { (icon("shield")) "exec off" } }
                span class="fragment-chev" { (icon("chevron-down")) }
            }
            div class="fragment-detail-body" {
                @if let Some(msg) = failed {
                    // The script ran but failed — show the error and a retry
                    // button right beneath it, in place of any output.
                    div class="fragment-run-error" {
                        div class="banner error" {
                            span class="banner-icon" { (icon("alert")) }
                            div class="banner-body" { "Script failed: " (msg) }
                        }
                        button type="button" class="btn btn-primary fragment-run-center"
                            hx-post=(run_url.clone()) hx-target="#profile-main" {
                            (icon("refresh")) "Retry"
                        }
                    }
                } @else if has_output {
                    pre class="fragment-output" { (c.markdown) }
                } @else if prompt {
                    // Centered run prompt — clicking it (or the corner button)
                    // re-renders this pane with the script's live output in place.
                    div class="fragment-run-prompt" {
                        button type="button" class="btn btn-primary fragment-run-center"
                            hx-post=(run_url) hx-target="#profile-main" {
                            (icon("play")) "Run script"
                        }
                        p class="run-hint muted" { "Runs this script and shows the live context it adds — output stays cached in the preview." }
                    }
                } @else {
                    div class="markdown-body" { (render_markdown(&c.markdown)) }
                }
                @if c.editable {
                    div class="fragment-detail-foot" {
                        button class="btn btn-ghost btn-sm"
                            hx-get=(format!("/fragments/{}/edit?profile={}", enc(&c.id), enc(profile)))
                            hx-target="#modal" { (icon("pencil")) "Edit fragment" }
                    }
                }
            }
        }
    }
}

fn profiles_empty_main() -> Markup {
    html! {
        div class="detail-blank" {
            (icon("layers"))
            p { "No loadouts yet." }
            p class="muted" { "A loadout bundles fragments and binds them to a kind of repo." }
            button class="btn btn-primary" hx-get="/profiles/new" hx-target="#main" { (icon("plus")) "Create your first loadout" }
        }
    }
}

fn profile_pick_prompt() -> Markup {
    html! {
        div class="detail-blank" {
            (icon("arrow-right"))
            p class="muted" { "Select a loadout to see what it composes." }
        }
    }
}

// --- Fragments tab --------------------------------------------------------

/// The Fragments tab: a grid of *your* fragment cards (open a dialog on
/// click). Only owned caps appear here — the shipped palette is a read-only
/// catalog you duplicate from when composing a profile, not an active layer.
pub fn fragments_tab(lib: &LibraryView, flash: Option<&str>) -> Markup {
    html! {
        div class="tab-fragments" {
            div class="dash-head" {
                h1 { "Fragments" }
                div class="head-actions" {
                    (legend())
                    button class="btn btn-primary" hx-get="/fragments/new" hx-target="#modal" { (icon("plus")) "New fragment" }
                }
            }
            @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
            @if lib.yours.is_empty() {
                div class="empty-card" {
                    p { "No fragments yet." }
                    p class="muted" { "A fragment is a reusable chunk of guidance (or a script) that loadouts compose. Write one here, or apply a Starter pack from the Loadouts tab to get a curated set plus a ready-made loadout." }
                    div class="empty-actions" {
                        button class="btn btn-primary" hx-get="/fragments/new" hx-target="#modal" { (icon("plus")) "Write your first fragment" }
                    }
                }
            } @else {
                @let groups = group_fragments(&lib.yours);
                @if groups.len() <= 1 {
                    div class="fragment-grid" { @for c in &lib.yours { (fragment_card(c)) } }
                } @else {
                    @for (label, caps) in &groups {
                        section class="fragment-group" {
                            h2 class="fragment-group-head" { (label) span class="fragment-group-count" { (caps.len()) } }
                            div class="fragment-grid" { @for c in caps { (fragment_card(c)) } }
                        }
                    }
                }
            }
        }
    }
}

pub fn fragments_tab_fragment(lib: &LibraryView, flash: Option<&str>) -> String {
    fragments_tab(lib, flash).into_string()
}

/// The Targets tab: the list of targets loadout can detect, each with the rule
/// that makes it work. Built-ins are read-only; the rule text is the answer to
/// "how does loadout decide a repo is this target?".
pub fn targets_tab(view: &TargetsView, flash: Option<&str>) -> Markup {
    html! {
        div class="tab-targets" {
            div class="dash-head" {
                h1 { "Targets" }
                div class="head-actions" {
                    button class="btn btn-primary" hx-get="/targets/new" hx-target="#modal" { (icon("plus")) "New target" }
                }
            }
            @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
            p class="muted targets-lead" {
                "A " strong { "target" } " is a label loadout attaches to a project by detecting it (a Rust repo, a Next.js app, …). A loadout applies to a repo when one of its targets matches. Built-in targets are read-only; add your own to recognize a project kind loadout doesn't yet. "
                span class="tag rec-tag" { (icon("check")) "matches here" }
                " marks the ones that match the repo studio is running in."
            }
            div class="target-list" {
                @for t in &view.targets { (target_row(t)) }
            }
        }
    }
}

pub fn targets_tab_fragment(view: &TargetsView) -> String {
    targets_tab(view, None).into_string()
}

// --- Workflows tab -----------------------------------------------------------

/// The Workflows tab: an always-visible gallery of curated + your own workflows
/// (tiny named cards across the top), and below it the focused one shown as its
/// ordered slots. Picking one makes it your single global active workflow.
pub fn workflows_tab(view: &WorkflowsView, flash: Option<&str>) -> Markup {
    let focused = view
        .focused_id
        .as_deref()
        .and_then(|id| view.workflows.iter().find(|w| w.id == id));
    html! {
        div class="tab-workflows" {
            div class="dash-head" {
                h1 { "Workflows" }
            }
            @if let Some(msg) = flash { p class="flash" { (icon("check")) (msg) } }
            p class="muted workflows-lead" {
                "Loadout gives your agent one fixed set of commands — "
                code { "/loadout:explore" } " · " code { "plan" } " · " code { "implement" } " · " code { "verify" }
                " — the steps of a good process. A " strong { "workflow" } " decides how each step works: you can plan like "
                "Boris or like Superpowers. Pick one below to make it active everywhere; the commands stay the same, what they do changes."
            }
            // The gallery: tiny named cards across the top, always visible.
            div class="wf-gallery" {
                @for w in &view.workflows {
                    (workflow_gallery_card(w, view.focused_id.as_deref() == Some(w.id.as_str())))
                }
                // Build-your-own: opens the blank editor (customize is secondary,
                // so it sits at the end of the gallery, not competing with the catalog).
                button class="wf-card wf-card-new" hx-get="/workflows/new" hx-target="#modal" title="Build your own workflow" {
                    span class="wf-card-top" {
                        span class="wf-card-glyph" { (icon("plus")) }
                        span class="wf-card-name" { "New workflow" }
                    }
                    span class="wf-card-blurb" { "Build your own" }
                }
            }
            // The focused workflow, shown as its ordered slots.
            @if let Some(w) = focused {
                (workflow_detail(w))
            } @else {
                p class="muted" { "No workflows available yet." }
            }
        }
    }
}

pub fn workflows_tab_fragment(view: &WorkflowsView) -> String {
    workflows_tab(view, None).into_string()
}

/// Re-render the Workflows tab after selecting an active workflow: the tab (with
/// a flash) plus a staged-changes indicator refresh.
pub fn workflows_result(view: &WorkflowsView, flash: &str) -> String {
    html! {
        (workflows_tab(view, Some(flash)))
        (staged_refresh())
    }
    .into_string()
}

/// One tiny gallery card: the workflow name + an active marker; click to focus.
fn workflow_gallery_card(w: &WorkflowView, focused: bool) -> Markup {
    let mut cls = String::from("wf-card");
    if focused {
        cls.push_str(" focused");
    }
    if w.active {
        cls.push_str(" active");
    }
    let glyph = w.icon.as_deref().unwrap_or("git-branch");
    html! {
        button class=(cls) hx-get=(format!("/workflows/{}", enc(&w.id))) hx-target="#main" {
            span class="wf-card-top" {
                span class="wf-card-glyph" { (icon(glyph)) }
                span class="wf-card-name" { (w.title) }
                @if w.active { span class="wf-card-dot" title="active workflow" { (icon("check")) } }
                @else if !w.builtin { span class="wf-card-tag" { "yours" } }
            }
            @if let Some(b) = &w.blurb { span class="wf-card-blurb" { (b) } }
        }
    }
}

/// The focused workflow in full: title + provenance + the slot spine + a "use
/// this" action that sets it as the global active workflow.
fn workflow_detail(w: &WorkflowView) -> Markup {
    html! {
        div class="wf-detail" {
            // The legend that makes the model legible: the steps are fixed
            // scaffolding; the accent-marked text is what THIS workflow puts in
            // each (the card already shows the name + blurb, so no repeat title).
            div class="wf-detail-head" {
                div class="wf-detail-titles" {
                    p class="wf-legend" {
                        "Fixed steps. The marked text is what "
                        span class="wf-legend-chip" { (icon(w.icon.as_deref().unwrap_or("git-branch"))) (w.title) }
                        " does at each — greyed steps it skips."
                    }
                    span class="wf-detail-meta muted" {
                        @if let Some(m) = &w.modeled_on { (m) }
                        @if let Some(s) = &w.source {
                            " · " a class="wf-source" href=(s) target="_blank" rel="noopener noreferrer" { (icon("globe")) "source" }
                        }
                        @if w.private { " · " span class="tag" { (icon("lock")) "private" } }
                    }
                }
                div class="wf-detail-actions" {
                    // Built-in → "Customize" (opens the editor prefilled; saving
                    // makes an owned copy). Owned → "Edit". Secondary styling, so
                    // the primary action stays "Use this workflow".
                    @if w.builtin {
                        button class="btn btn-ghost" hx-get=(format!("/workflows/{}/customize", enc(&w.id))) hx-target="#modal" title="Duplicate into a workflow you can edit" { (icon("copy")) "Customize" }
                    } @else {
                        button class="btn btn-ghost" hx-get=(format!("/workflows/{}/edit", enc(&w.id))) hx-target="#modal" { (icon("pencil")) "Edit" }
                    }
                    @if w.active {
                        span class="tag rec-tag wf-active-pill" { (icon("check")) "active workflow" }
                    } @else {
                        button class="btn btn-primary" hx-post=(format!("/workflows/{}/activate", enc(&w.id))) hx-target="#main" { (icon("check")) "Use this workflow" }
                    }
                }
            }
            @for prob in &w.problems { p class="flash flash-warn" { (icon("alert")) (prob) } }
            // The fixed slots — same five for every workflow. Each carries its
            // own step icon + name; the active workflow's icon marks the value.
            ul class="wf-slots" {
                @for s in &w.slots { (workflow_slot(s, w.icon.as_deref().unwrap_or("git-branch"))) }
            }
            @if !w.bound_by.is_empty() {
                div class="wf-detail-foot" {
                    span class="muted wf-foot-item" { (icon("layers")) "Pinned on loadout: " (w.bound_by.join(", ")) }
                }
            }
        }
    }
}

/// One slot: its own step icon + large name + the slash command (the fixed part,
/// identical across workflows), then the active workflow's contribution — its
/// text marked with the workflow's own icon (`wf_glyph`) so the value is plainly
/// tied to the selection. A skipped slot greys out and shows only the generic
/// step description.
fn workflow_slot(s: &WorkflowSlotView, wf_glyph: &str) -> Markup {
    let cls = if s.filled {
        "wf-slot"
    } else {
        "wf-slot skipped"
    };
    html! {
        li class=(cls) {
            // The fixed identity — step icon + name + command. Same in every
            // workflow, so it's the stable scaffolding.
            div class="wf-slot-head" {
                span class="wf-slot-icon" { (icon(&s.icon)) }
                div class="wf-slot-titles" {
                    span class="wf-slot-name" { (s.name) }
                    code class="wf-slot-cmd" {
                        span class="cmd-prefix" { "/loadout:" }
                        span class="cmd-name" { (s.command) }
                    }
                }
            }
            @match &s.purpose {
                // Filled — a tinted panel (the workflow's contribution) that
                // grows to fill the card and vertically centers its content.
                Some(p) => div class="wf-value" {
                    div class="wf-value-row" {
                        @if s.edited {
                            span class="wf-value-mark wf-value-edited" title="you customized this step" { (icon("pencil")) }
                        } @else {
                            span class="wf-value-mark" title="from this workflow" { (icon(wf_glyph)) }
                        }
                        div class="wf-value-body" {
                            p class="wf-value-text" { (p) }
                            @if s.reads.is_some() || s.writes.is_some() {
                                div class="wf-slot-io" {
                                    @if let Some(r) = &s.reads { span class="stage-io" { (icon("file")) "reads " code { (r) } } }
                                    @if let Some(wr) = &s.writes { span class="stage-io" { (icon("file")) "writes " code { (wr) } } }
                                }
                            }
                            @if !s.exit.is_empty() {
                                ul class="stage-exit" {
                                    @for item in &s.exit { li { (icon("check")) (item) } }
                                }
                            }
                        }
                    }
                },
                // Skipped — just the generic step, greyed + centered, so you know
                // what it is without it competing with the filled steps.
                None => span class="wf-step-desc" { (s.step_desc) },
            }
        }
    }
}

/// The workflow editor modal. `base` is the workflow it starts from (`None` for
/// a blank one); `customize` means "duplicate `base` into a new workflow" (the
/// original is left intact) rather than editing an owned one in place.
///
/// The five fixed steps are a tab row; selecting one opens a big instructions
/// box for that step — a step is just markdown. The handoff files / checkpoint /
/// checklist a built-in carries aren't edited here; they ride along from `base`
/// when you save (see [`crate::studio::state::workflow_from_form`]). The card
/// icon isn't chosen — it's inherited from the source (default glyph for new).
pub fn workflow_editor(base: Option<&crate::workflow::Workflow>, customize: bool) -> String {
    use crate::studio::state::{slot_display_name, slot_icon, MAX_WORKFLOW_EXTRAS};
    use crate::workflow::{WorkflowStage, CANONICAL_SLOTS};

    let layout = base.map(|b| b.canonical_layout());
    let stage_for = |key: &str| -> Option<&WorkflowStage> {
        layout.as_ref().and_then(|l| {
            l.slots
                .iter()
                .find(|s| s.command == key)
                .and_then(|s| s.stage)
        })
    };
    let extras: Vec<&WorkflowStage> = layout
        .as_ref()
        .map(|l| l.extras.clone())
        .unwrap_or_default();
    let extra_rows = (extras.len() + 1).min(MAX_WORKFLOW_EXTRAS);

    // Edit (owned, in place) vs new/customize (creates a fresh workflow).
    let is_edit = base.is_some() && !customize;
    let from = base.map(|b| b.id.as_str()).unwrap_or("");
    let desc = base.and_then(|b| b.description.as_deref()).unwrap_or("");
    // Customize prefills a distinct "<name> copy" so the original id is untouched.
    let name_value = if customize {
        format!("{} copy", base.map(|b| b.title()).unwrap_or("Workflow"))
    } else {
        base.and_then(|b| b.name.clone()).unwrap_or_default()
    };
    let heading = if customize {
        format!(
            "Customize {}",
            base.map(|b| b.title()).unwrap_or("workflow")
        )
    } else if is_edit {
        format!("Edit {}", base.map(|b| b.title()).unwrap_or("workflow"))
    } else {
        "New workflow".to_string()
    };

    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal modal-lg" {
            form class="fragment-form workflow-form" hx-post="/workflows" hx-target="#main" {
                div class="modal-head" {
                    h2 { (heading) }
                    (close_btn())
                }
                div class="modal-body" {
                    // `mode` = edit (in place) or new (create). `from` names the
                    // workflow this one copies its handoffs/provenance from.
                    input type="hidden" name="mode" value=(if is_edit { "edit" } else { "new" });
                    input type="hidden" name="from" value=(from);
                    @if customize {
                        p class="hint" { "This makes a separate copy you can edit — “" (base.map(|b| b.title()).unwrap_or("the original")) "” stays as it is." }
                    }
                    div class="wf-edit-meta" {
                        @if is_edit { input type="hidden" name="id" value=(from); }
                        label class="field grow" { span class="field-label" { "name" @if !is_edit { span class="field-hint" { "becomes the workflow id" } } }
                            input type="text" name="name" value=(name_value) placeholder="My workflow" required;
                        }
                        label class="field grow" { span class="field-label" { "description" span class="field-hint" { "optional one-liner" } }
                            input type="text" name="description" value=(desc) placeholder="what this workflow is for";
                        }
                    }

                    p class="wf-edit-lead muted" {
                        "Pick a step, then write what the agent should do there — plain text is fine. "
                        "Leave a step blank to skip it; handoff files between steps carry over from the original."
                    }
                    // The five fixed steps as a tab row (CSS-only): each radio's
                    // label is a tab, and the matching panel below shows its big
                    // instructions box. Every panel stays in the form, so all
                    // steps submit regardless of which tab is open.
                    div class="wf-steps" {
                        @for (i, &(key, _desc)) in CANONICAL_SLOTS.iter().enumerate() {
                            @let filled = stage_for(key).and_then(|s| s.purpose.as_deref()).map(|p| !p.trim().is_empty()).unwrap_or(false);
                            input type="radio" name="wf_step" class="wf-step-radio" id=(format!("wfstep-{key}")) checked[i == 0];
                            label class=(if filled { "wf-step-tab filled" } else { "wf-step-tab" }) for=(format!("wfstep-{key}")) {
                                span class="wf-step-tab-icon" { (icon(slot_icon(key))) }
                                span { (slot_display_name(key)) }
                                @if filled { span class="wf-step-dot" title="has instructions" {} }
                            }
                        }
                        @for &(key, desc) in CANONICAL_SLOTS {
                            @let st = stage_for(key);
                            div class="wf-step-panel" id=(format!("wfpanel-{key}")) {
                                div class="wf-step-panel-head" {
                                    span class="wf-slot-icon" { (icon(slot_icon(key))) }
                                    div class="wf-slot-titles" {
                                        span class="wf-slot-name" { (slot_display_name(key)) }
                                        code class="wf-slot-cmd" { span class="cmd-prefix" { "/loadout:" } span class="cmd-name" { (key) } }
                                    }
                                }
                                textarea name=(format!("s_{key}_purpose")) class="wf-step-textarea" placeholder=(desc) {
                                    (st.and_then(|s| s.purpose.as_deref()).unwrap_or(""))
                                }
                            }
                        }
                    }

                    details class="wf-edit-extras" {
                        summary { "Custom steps " span class="field-hint" { "optional — beyond the five" } }
                        @for i in 0..extra_rows {
                            @let ex = extras.get(i).copied();
                            div class="wf-edit-extra" {
                                label class="field" { span class="field-label" { "name" }
                                    input type="text" name=(format!("x{i}_name")) value=(ex.map(|s| s.name.as_str()).unwrap_or("")) placeholder="e.g. compound";
                                }
                                label class="field grow" { span class="field-label" { "instructions" }
                                    input type="text" name=(format!("x{i}_purpose")) value=(ex.and_then(|s| s.purpose.as_deref()).unwrap_or("")) placeholder="what this step does";
                                }
                            }
                        }
                    }
                }
                div class="modal-foot" {
                    @if is_edit {
                        button type="button" class="btn btn-danger delete-left"
                            hx-delete=(format!("/workflows/{}", enc(from))) hx-target="#main"
                            hx-confirm=(format!("Delete workflow “{from}”? This stages its removal.")) {
                            (icon("trash")) "Delete"
                        }
                    }
                    button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                    button type="submit" class="btn btn-primary" { (icon("check")) "Save" }
                }
            }
        }
    }
    .into_string()
}

/// Re-render the Targets tab after a staged edit: the tab (with a flash), close
/// the modal, and refresh the staged-changes indicator.
pub fn target_result(view: &TargetsView, flash: &str) -> String {
    html! {
        (targets_tab(view, Some(flash)))
        (modal_close())
        (staged_refresh())
    }
    .into_string()
}

/// One row in the Targets list: id, what it is, and the detection rule. Custom
/// (editable) targets carry an edit affordance; built-ins are read-only.
fn target_row(t: &TargetView) -> Markup {
    // The target's own icon (built-in glyph or custom pick), falling back to a
    // lettermark; a no-icon script target defaults to the terminal glyph so its
    // executable nature still reads at a glance.
    let token = t
        .icon
        .as_deref()
        .or_else(|| t.is_script.then_some("terminal"));
    html! {
        div class="target-row" {
            span class="target-glyph" { (target_icon_markup(&resolve_target_icon(token, &t.id))) }
            div class="target-main" {
                span class="target-top" {
                    span class="target-id" { (t.id) }
                    @if t.builtin { span class="tag" { "built-in" } }
                    @if t.private { span class="tag" { (icon("lock")) "private" } }
                    @if t.detected { span class="tag rec-tag" { (icon("check")) "matches here" } }
                }
                @if let Some(d) = &t.description { span class="target-desc" { (d) } }
                span class="target-rule" { (icon("eye")) "Detected when " code class="rule-code" { (t.rule_summary) } }
            }
            @if t.editable {
                button class="btn btn-ghost btn-sm target-edit"
                    hx-get=(format!("/targets/{}/edit", enc(&t.id))) hx-target="#modal"
                    title="Edit target" { (icon("pencil")) }
            }
        }
    }
}

/// The simple-editor field values for a custom-target rule.
#[derive(Default)]
struct TargetForm {
    kind: &'static str,
    paths: String,
    contains_path: String,
    contains_value: String,
    command: String,
    lang: String,
    allow_exec: bool,
}

/// Map a rule to the editor's fields, or `None` when it's too rich for the form
/// (an all-of, or a composite that isn't a plain any-of-files) and must be
/// hand-edited as TOML.
fn target_form_fields(rule: &TargetRule) -> Option<TargetForm> {
    match rule {
        TargetRule::FileExists { path } => Some(TargetForm {
            kind: "file_exists",
            paths: path.clone(),
            ..Default::default()
        }),
        TargetRule::AnyOf { rules }
            if !rules.is_empty()
                && rules
                    .iter()
                    .all(|r| matches!(r, TargetRule::FileExists { .. })) =>
        {
            let paths = rules
                .iter()
                .filter_map(|r| match r {
                    TargetRule::FileExists { path } => Some(path.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(", ");
            Some(TargetForm {
                kind: "file_exists",
                paths,
                ..Default::default()
            })
        }
        TargetRule::FileContains { path, value, .. } => Some(TargetForm {
            kind: "file_contains",
            contains_path: path.clone(),
            contains_value: value.clone(),
            ..Default::default()
        }),
        TargetRule::Script {
            command,
            script_lang,
            allow_exec,
            ..
        } => Some(TargetForm {
            kind: "script",
            command: command.clone(),
            lang: script_lang.clone().unwrap_or_else(|| "bash".to_string()),
            allow_exec: *allow_exec,
            ..Default::default()
        }),
        _ => None,
    }
}

/// The custom-target icon control: a grid of glyph tiles plus a leading
/// "lettermark" tile (the default) that shows the badge derived from the id. The
/// selected tile posts `icon=<glyph>` (or empty for the lettermark). `current` is
/// the target's stored icon; `id` seeds the lettermark preview.
fn target_icon_picker(current: Option<&str>, id: &str) -> Markup {
    let cur = current.map(str::trim).filter(|s| !s.is_empty());
    // The auto tile previews the derived letters when editing a named target; a
    // brand-new target (no name yet) shows a neutral "Aa" placeholder.
    let mark = if id.is_empty() {
        "Aa".to_string()
    } else {
        crate::target::lettermark(id)
    };
    html! {
        div class="field icon-pick-field" {
            span class="field-label" { "icon" span class="field-hint" { "shown on loadouts — pick a glyph, or use a lettermark from the name" } }
            div class="icon-picker" {
                input type="radio" name="icon" id="ic-auto" value="" checked[cur.is_none()];
                label class="icon-opt" for="ic-auto" title="Lettermark (from the name)" {
                    span class="lettermark" { (mark) }
                }
                @for &g in BRAND_ICONS.iter().chain(GENERIC_GLYPHS) {
                    @let gid = format!("ic-{g}");
                    input type="radio" name="icon" id=(gid) value=(g) checked[cur == Some(g)];
                    label class="icon-opt" for=(gid) title=(g) { (target_icon_markup(&resolve_target_icon(Some(g), g))) }
                }
            }
        }
    }
}

/// The custom-target editor modal (create or edit). Built-in targets are never
/// passed here (they're read-only). A target whose rule the simple form can't
/// represent gets a "hand-edit as TOML" notice instead.
pub fn target_dialog(target: Option<&TargetDef>, layer: Layer) -> String {
    let is_new = target.is_none();
    let id = target.map(|t| t.id.as_str()).unwrap_or("");
    let fields = target.map(|t| target_form_fields(&t.rule));
    // Editing a rule the simple form can't show: send the user to the TOML.
    if let Some(None) = fields {
        return html! {
            div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
            div class="modal" {
                div class="modal-head" { h2 { "Advanced target" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "“" (id) "” uses a rule the quick editor can't show (a composite all-of/any-of). Edit it directly in your config TOML." }
                }
                div class="modal-foot" { button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" } }
            }
        }
        .into_string();
    }
    let f = fields.flatten().unwrap_or(TargetForm {
        kind: "file_exists",
        lang: "bash".to_string(),
        allow_exec: true,
        ..Default::default()
    });
    let desc = target.and_then(|t| t.description.as_deref()).unwrap_or("");
    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal" {
            form class="fragment-form target-form" hx-post="/targets" hx-target="#main" {
                div class="modal-head" {
                    h2 { (if is_new { "New target" } else { "Edit target" }) }
                    (close_btn())
                }
                div class="modal-body" {
                    @if !is_new { input type="hidden" name="id" value=(id); }
                    label class="field grow" { span class="field-label" { "name" span class="field-hint" { "the label loadouts target, e.g. deno" } }
                        input type="text" name="name" value=(if is_new { "" } else { id }) placeholder="deno" required[is_new] readonly[!is_new];
                    }
                    label class="field grow" { span class="field-label" { "description" span class="field-hint" { "optional" } }
                        input type="text" name="description" value=(desc) placeholder="a Deno project";
                    }
                    (target_icon_picker(target.and_then(|t| t.icon.as_deref()), id))
                    div class="seg" {
                        input type="radio" name="kind" id="tkind-fe" value="file_exists" checked[f.kind == "file_exists"];
                        label class="seg-opt" for="tkind-fe" { "File exists" }
                        input type="radio" name="kind" id="tkind-fc" value="file_contains" checked[f.kind == "file_contains"];
                        label class="seg-opt" for="tkind-fc" { "File contains" }
                        input type="radio" name="kind" id="tkind-sc" value="script" checked[f.kind == "script"];
                        label class="seg-opt" for="tkind-sc" { "Script" }
                    }
                    div class="kind-fe" {
                        label class="field" { span class="field-label" { "file(s)" span class="field-hint" { "comma-separated; matches if any exists" } }
                            input type="text" name="paths" value=(f.paths) placeholder="deno.json, deno.jsonc";
                        }
                    }
                    div class="kind-fc" {
                        label class="field" { span class="field-label" { "file" }
                            input type="text" name="contains_path" value=(f.contains_path) placeholder="pyproject.toml";
                        }
                        label class="field" { span class="field-label" { "contains text" }
                            input type="text" name="contains_value" value=(f.contains_value) placeholder="django";
                        }
                    }
                    div class="kind-sc" {
                        div class="script-head" {
                            label class="field grow" { span class="field-label" { "script" span class="field-hint" { "exit 0 = match; runs in the repo" } } }
                            div class="seg seg-sm" {
                                @for (val, lbl) in SCRIPT_LANGS {
                                    @let lid = format!("tlang-{val}");
                                    input type="radio" name="script_lang" id=(lid) value=(val) checked[f.lang == *val];
                                    label class="seg-opt" for=(lid) { (lbl) }
                                }
                            }
                        }
                        div class="code-edit-wrap" {
                            pre class="code-hl" aria-hidden="true" { code {} }
                            textarea name="command" rows="6" class="mono code-edit" spellcheck="false" placeholder="test -f deno.json" { (f.command) }
                        }
                        div class="script-actions" {
                            label class="check exec-check" { input type="checkbox" name="allow_exec" checked[is_new || f.allow_exec]; span { "Allow execution" } }
                            button type="button" class="btn btn-ghost btn-sm script-try"
                                hx-post="/targets/try" hx-target="#target-tryout"
                                title="Run this predicate now against the repo (nothing is saved)" {
                                (icon("play")) "Run"
                            }
                        }
                        div id="target-tryout" class="script-tryout" {}
                        p class="hint small" { "The predicate runs at detection (only on real renders), cwd set to the repo; its verdict is cached. Uncheck " strong { "Allow execution" } " to disable it." }
                    }
                    (lives_in(layer))
                    p class="hint small" { "Detected against each repo at render. A loadout whose targets include this id applies wherever it matches." }
                }
                div class="modal-foot" {
                    @if !is_new {
                        button type="button" class="btn btn-danger delete-left"
                            hx-delete=(format!("/targets/{}", enc(id))) hx-target="#main"
                            hx-confirm=(format!("Delete target “{id}”? This stages its removal.")) {
                            (icon("trash")) "Delete"
                        }
                    }
                    button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                    button type="submit" class="btn btn-primary" { (icon("check")) "Save" }
                }
            }
        }
    }
    .into_string()
}

/// Order known categories sensibly; unknown categories fall after them
/// (alphabetical), and the uncategorized "General" bucket sorts last. Lists the
/// friendly `category` values first, then the legacy first-tag fallback keys.
const CATEGORY_ORDER: &[&str] = &[
    // friendly `category` values (the dedicated field)
    "Operating Style",
    "Local Environment",
    "Stack Conventions",
    "Dev Workflow",
    "Engineering Standards",
    "Quality",
    "Safety",
    "Security",
    // legacy first-tag fallback (fragments with no explicit category)
    "awareness",
    "stack",
    "comms",
    "dev-workflow",
    "quality",
    "infra",
    "safety",
    "security",
];

/// Friendly category names offered as autocomplete in the fragment editor.
const CATEGORY_SUGGESTIONS: &[&str] = &[
    "Operating Style",
    "Local Environment",
    "Stack Conventions",
    "Dev Workflow",
    "Engineering Standards",
    "Quality",
    "Safety",
    "Security",
];

/// A friendly heading for a fragment category (its primary tag).
fn category_label(cat: Option<&str>) -> String {
    match cat {
        Some("stack") => "Stack conventions".to_string(),
        Some("comms") => "Communication".to_string(),
        Some("awareness") => "Awareness".to_string(),
        Some("infra") => "Infrastructure".to_string(),
        Some("safety") => "Safety".to_string(),
        Some("security") => "Security".to_string(),
        Some("quality") => "Quality".to_string(),
        Some("dev-workflow") => "Workflow".to_string(),
        Some(other) => title_case(other),
        None => "General".to_string(),
    }
}

/// "my-tag" → "My tag" for an unmapped category.
fn title_case(s: &str) -> String {
    let spaced = s.replace(['-', '_'], " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// Group fragments by their primary category, in a stable, friendly order.
/// Within a group, the caps keep their library order.
fn group_fragments(caps: &[FragmentView]) -> Vec<(String, Vec<&FragmentView>)> {
    let key_of = |c: &FragmentView| c.category.clone().unwrap_or_default();
    // Distinct keys in first-seen order, then sorted by rank.
    let mut keys: Vec<String> = Vec::new();
    for c in caps {
        let k = key_of(c);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    keys.sort_by_key(|k| category_rank(k));
    keys.into_iter()
        .map(|k| {
            let label = category_label(if k.is_empty() { None } else { Some(&k) });
            let members: Vec<&FragmentView> = caps.iter().filter(|c| key_of(c) == k).collect();
            (label, members)
        })
        .collect()
}

/// Sort key: known categories by their `CATEGORY_ORDER` index, then unknown
/// categories alphabetically, then the uncategorized "General" bucket last.
fn category_rank(key: &str) -> (u8, String) {
    if key.is_empty() {
        return (3, String::new());
    }
    match CATEGORY_ORDER.iter().position(|&c| c == key) {
        Some(i) => (1, format!("{i:02}")),
        None => (2, key.to_string()),
    }
}

fn fragment_card(c: &FragmentView) -> Markup {
    let id = c.id.as_str();
    let e = enc(id);
    html! {
        div class="fragment-card" hx-get=(format!("/fragments/{e}/edit")) hx-target="#modal" role="button" tabindex="0" {
            // `k-{kind}` (static/command/provider) colors the glyph tile so
            // executable fragments (scripts + live providers) read distinctly.
            span class=(format!("fragment-glyph k-{}", c.kind)) { (icon(fragment_icon_name(c))) }
            div class="fragment-main" {
                span class="fragment-title" { (c.title) }
                @if let Some(s) = &c.summary { span class="fragment-summary" { (s) } }
                span class="fragment-id" { (id) }
            }
            // The glyph already conveys the type; only flag the exceptions —
            // a private (local.toml) fragment. Shared is the unmarked default.
            @if c.private {
                div class="fragment-tags" {
                    span class="tag" { (icon("lock")) "private" }
                }
            }
        }
    }
}

// --- starter packs + legend --------------------------------------------------

/// A compact, collapsible key to studio's visual language: the type glyphs, the
/// private flag, and the profile/pack atom-dot states.
fn legend() -> Markup {
    html! {
        details class="legend" {
            summary { (icon("eye")) "Legend" }
            div class="legend-body" {
                div class="legend-group" {
                    span class="legend-head" { "Type" }
                    span class="legend-row" { span class="fragment-glyph k-static" { (icon("file")) } "markdown" }
                    span class="legend-row" { span class="fragment-glyph k-command" { (icon("terminal")) } "script — runs at render" }
                    span class="legend-row" { span class="fragment-glyph k-provider" { (icon("bolt")) } "live provider" }
                    span class="legend-row" { span class="tag" { (icon("lock")) "private" } "local.toml" }
                }
                div class="legend-group" {
                    span class="legend-head" { "Fragment dots" }
                    span class="legend-row" { span class="atom owned" {} "owned — composes" }
                    span class="legend-row" { span class="atom palette" {} "palette only" }
                    span class="legend-row" { span class="atom unknown" {} "unknown id" }
                }
            }
        }
    }
}

/// The starter-pack gallery (`#main`): a header, the legend, and a grid of pack
/// cards (recommended first). Applying a card stages the pack's caps + profile.
pub fn packs_gallery(packs: &[PackView]) -> Markup {
    html! {
        div class="tab-packs" {
            div class="dash-head" {
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { "Starter packs" }
                }
                (legend())
            }
            p class="muted gallery-lead" { "A pack copies a curated set of fragments into your library and creates a ready-made loadout — all staged for you to review and Apply. " strong { "Preview" } " any pack first, and customize it freely once added." }
            div class="pack-grid" { @for p in packs { (pack_card(p)) } }
        }
    }
}

pub fn packs_gallery_fragment(packs: &[PackView]) -> String {
    packs_gallery(packs).into_string()
}

/// One starter-pack card: icon + name (+ recommended/applied badge), a short
/// description, the composed fragments as atom dots, and an
/// Apply action (disabled once the pack's profile already exists).
fn pack_card(p: &PackView) -> Markup {
    let e = enc(&p.id);
    let mut cls = String::from("pack-card");
    if p.recommended {
        cls.push_str(" recommended");
    }
    if p.applied {
        cls.push_str(" applied");
    }
    html! {
        div class=(cls) {
            div class="pack-head" {
                span class="pack-glyph" { (icon(&p.icon)) }
                span class="pack-name" { (p.name) }
                @if p.recommended { span class="tag rec-tag" { (icon("check")) "recommended" } }
            }
            p class="pack-desc" { (p.description) }
            div class="pack-foot" {
                span class="atoms" { @for a in &p.atoms { (atom_dot(a)) } }
                span class="muted small" { (p.atoms.len()) " fragments" }
                span class="pack-spacer" {}
                button class="btn btn-ghost btn-sm" hx-get=(format!("/packs/{e}/preview")) hx-target="#modal" { (icon("eye")) "Preview" }
                @if p.applied {
                    button class="btn btn-ghost btn-sm" disabled { (icon("check")) "Applied" }
                } @else {
                    button class="btn btn-primary btn-sm" hx-post=(format!("/packs/{e}/apply")) hx-target="#main" { (icon("plus")) "Apply" }
                }
            }
        }
    }
}

/// The starter-pack preview modal: the pack's profile rendered as a full
/// document, each composed fragment demarcated by its glyph + title + id, plus a
/// note that the profile is fully customizable once added.
pub fn pack_preview(pack: &crate::pack::Pack, outcome: &PreviewOutcome) -> String {
    let e = enc(pack.id);
    html! {
        div class="modal-root" {
            div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
            div class="modal" {
                div class="modal-head" {
                    h2 { "Preview · " (pack.name) }
                    (close_btn())
                }
                div class="modal-body" {
                    p class="muted" {
                        "Applying stages " (outcome.caps.len()) " fragments and the "
                        strong { (pack.profile_name) } " loadout. You review the diff before "
                        "anything is saved — and can edit, add, or remove any of it afterward."
                    }
                    @if outcome.caps.is_empty() {
                        p class="empty-card muted" { "This pack composes nothing in the current context." }
                    } @else {
                        div class="pack-preview-doc" {
                            @for c in &outcome.caps {
                                section class="pack-preview-frag" {
                                    div class="pack-preview-frag-head" {
                                        span class="fragment-glyph" { (icon(c.glyph)) }
                                        span class="pack-preview-frag-title" { (c.title) }
                                        span class="fragment-id" { (c.id) }
                                    }
                                    div class="markdown-body" { (render_markdown(&c.markdown)) }
                                }
                            }
                        }
                    }
                }
                div class="modal-foot" {
                    button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" }
                    @if !outcome.caps.is_empty() {
                        button type="button" class="btn btn-primary" hx-post=(format!("/packs/{e}/apply")) hx-target="#main" { (icon("plus")) "Apply" }
                    }
                }
            }
        }
    }
    .into_string()
}

// --- guided onboarding beats -------------------------------------------------

/// Pluralize a count: `n` + a singular/plural noun ("1 fragment" / "3 fragments").
fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Beat 2 of the guided first-run: after a starter pack is staged, a friendly
/// "review what will change" summary (counts, not raw diffs) that stresses
/// nothing is written yet — with an escape hatch to the exact unified diff.
pub fn onboarding_review(summary: &crate::studio::state::StagedSummary) -> String {
    html! {
        div class="onboard onboard-review" {
            div class="onboard-head" {
                span class="onboard-badge" { (icon("eye")) }
                h1 { "Review what will change" }
            }
            p class="muted" { "Nothing is written to disk yet. Applying will add:" }
            ul class="onboard-summary" {
                @if summary.fragments_added > 0 {
                    li {
                        span class="fragment-glyph" { (icon("file")) }
                        (plural(summary.fragments_added, "fragment", "fragments"))
                    }
                }
                @for p in &summary.profiles {
                    li {
                        span class="fragment-glyph" { (icon("layers")) }
                        "loadout " strong { (p.name) }
                        @if !p.targets.is_empty() {
                            span class="welcome-chips" {
                                @for t in &p.targets { @let id = t.as_str(); (target_chip(id, crate::target::builtin_icon(id))) }
                            }
                        }
                    }
                }
            }
            div class="onboard-actions" {
                button class="btn btn-primary" hx-post="/apply" hx-target="#main" { (icon("check")) "Apply" }
                button class="btn btn-ghost" hx-get="/diff" hx-target="#main" { (icon("eye")) "See exact diff" }
                button class="btn btn-ghost" hx-post="/discard" hx-target="#main"
                    hx-confirm="Discard staged changes and start over?" { (icon("x")) "Start over" }
            }
        }
    }
    .into_string()
}

/// Beat 3 of the guided first-run: after Apply, confirm the setup is live and —
/// the piece that was missing — name the one command that actually uses it
/// (`load run <agent>`) plus how to reopen the studio.
pub fn onboarding_done(summary: &crate::studio::state::StagedSummary, agent: &str) -> String {
    let targets: Vec<&String> = summary
        .profiles
        .iter()
        .flat_map(|p| p.targets.iter())
        .collect();
    html! {
        div class="onboard onboard-done" {
            div class="onboard-head" {
                span class="onboard-badge ok" { (icon("check")) }
                h1 { "You're set" }
            }
            p class="welcome-lead" {
                "Your guidance is live. When you launch an AI agent in a matching repo, "
                "loadout injects it automatically — no per-project setup."
            }
            @if !targets.is_empty() {
                div class="welcome-detect" {
                    span class="muted small" { "active for" }
                    span class="welcome-chips" {
                        @for t in &targets { @let id = t.as_str(); (target_chip(id, crate::target::builtin_icon(id))) }
                    }
                }
            }
            div class="cmd-block" {
                span class="muted small" { "Use it in any agent session:" }
                code { "load run " (agent) }
            }
            div class="cmd-block" {
                span class="muted small" { "Reopen this studio anytime:" }
                code { "load studio" }
            }
            div class="onboard-actions" {
                button class="btn btn-primary" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) "Explore your setup" }
            }
        }
    }
    .into_string()
}

// --- fragment dialog (modal) ----------------------------------------------

/// The fragment dialog content (swapped into `#modal`). A palette item is
/// read-only with a duplicate action; an advanced cap is read-only with an
/// "edit in TOML" note; otherwise the content-first editor.
pub fn fragment_dialog(
    cap: Option<&Fragment>,
    layer: Layer,
    owned: bool,
    return_profile: Option<&str>,
    used_by: &[String],
) -> String {
    let is_new = cap.is_none();
    let id = cap.map(|c| c.id.as_str()).unwrap_or("");
    let read_only_palette = !is_new && !owned;
    let advanced = cap
        .map(crate::studio::state::is_advanced_fragment)
        .unwrap_or(false);
    // Deleting a composed fragment also cleans it out of the profiles using it;
    // warn up front and name them so it isn't a surprise.
    let delete_confirm = if used_by.is_empty() {
        format!("Delete fragment “{id}”? This stages its removal.")
    } else {
        let names = used_by
            .iter()
            .map(|n| format!("“{n}”"))
            .collect::<Vec<_>>()
            .join(", ");
        let those = if used_by.len() == 1 {
            "that loadout"
        } else {
            "those loadouts"
        };
        format!(
            "Delete fragment “{id}”? It's composed by {names} — deleting it will also remove it from {those}. This stages all the changes."
        )
    };
    html! {
        div class="modal-backdrop" hx-get="/close" hx-target="#modal" {}
        div class="modal" {
            @if read_only_palette {
                div class="modal-head" { h2 { "Palette fragment" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "Starter template. Duplicate “" (id) "” into your library to own and edit it." }
                }
                div class="modal-foot" {
                    button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" }
                    button class="btn btn-primary" hx-post=(format!("/fragments/{}/duplicate", enc(id))) hx-target="#main" { (icon("copy")) "Duplicate into my library" }
                }
            } @else if advanced {
                div class="modal-head" { h2 { "Advanced fragment" } (close_btn()) }
                div class="modal-body" {
                    p class="hint" { "“" (id) "” uses features the quick editor can't show without dropping one side (a built-in provider, or a script with a custom template). Edit it directly in your config TOML." }
                }
                div class="modal-foot" { button class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Close" } }
            } @else {
                @let is_script = cap.map(|c| c.command.is_some()).unwrap_or(false);
                @let allow_exec = cap.map(|c| c.allow_exec).unwrap_or(true);
                @let lang = cap.and_then(|c| c.script_lang.as_deref()).unwrap_or("bash");
                form class="fragment-form" hx-post="/fragments" hx-target="#main" {
                    div class="modal-head" {
                        h2 { (if is_new { "New fragment" } else { "Edit fragment" }) }
                        (close_btn())
                    }
                    div class="modal-body" {
                        @if !is_new { input type="hidden" name="id" value=(id); }
                        @if let Some(rp) = return_profile { input type="hidden" name="return_profile" value=(rp); }
                        label class="field grow" { span class="field-label" { "title" }
                            input type="text" name="name" value=(cap.and_then(|c| c.description.as_deref()).unwrap_or(id)) placeholder="Rust conventions" required;
                        }
                        div class="seg" {
                            input type="radio" name="kind" id="kind-md" value="markdown" checked[!is_script];
                            label class="seg-opt" for="kind-md" { "Markdown" }
                            input type="radio" name="kind" id="kind-sc" value="script" checked[is_script];
                            label class="seg-opt" for="kind-sc" { "Script" }
                        }
                        div class="kind-md" {
                            label class="field" { span class="field-label" { "content" span class="field-hint" { "markdown" } }
                                textarea name="guidance" rows="9" placeholder="# Rust conventions&#10;Build with cargo; lint with clippy." { (cap.map(|c| c.guidance.as_str()).unwrap_or("")) }
                            }
                        }
                        div class="kind-sc" {
                            div class="script-head" {
                                label class="field grow" { span class="field-label" { "script" span class="field-hint" { "its output is embedded" } } }
                                div class="seg seg-sm" {
                                    @for (val, lbl) in SCRIPT_LANGS {
                                        @let lid = format!("lang-{val}");
                                        input type="radio" name="script_lang" id=(lid) value=(val) checked[lang == *val];
                                        label class="seg-opt" for=(lid) { (lbl) }
                                    }
                                }
                            }
                            div class="code-edit-wrap" {
                                pre class="code-hl" aria-hidden="true" { code {} }
                                textarea name="command" rows="7" class="mono code-edit" spellcheck="false" placeholder="echo 'last deploy: green'" { (cap.and_then(|c| c.command.as_deref()).unwrap_or("")) }
                            }
                            div class="script-actions" {
                                label class="check exec-check" { input type="checkbox" name="allow_exec" checked[allow_exec]; span { "Allow execution" } }
                                button type="button" class="btn btn-ghost btn-sm script-try"
                                    hx-post="/fragments/try" hx-target="#script-tryout"
                                    title="Run this script now and show its output (nothing is saved)" {
                                    (icon("play")) "Run"
                                }
                            }
                            // Empty until the user clicks Run; `.script-tryout:empty`
                            // is hidden so this adds no noise to the editor.
                            div id="script-tryout" class="script-tryout" {}
                            p class="hint small" { "The script runs at render and its output is embedded. Uncheck " strong { "Allow execution" } " to keep it from running. " strong { "Run" } " tests it now without saving." }
                        }
                        @let cur_category = cap.and_then(|c| c.category.as_deref()).unwrap_or("");
                        div class="meta-row" {
                            label class="field grow" { span class="field-label" { "category" span class="field-hint" { "groups it in the tree" } }
                                input type="text" name="category" value=(cur_category) placeholder="Operating Style" list="fragment-categories";
                            }
                        }
                        datalist id="fragment-categories" {
                            @for c in CATEGORY_SUGGESTIONS { option value=(c) {} }
                        }
                        (lives_in(layer))
                        @if !is_new {
                            p class="hint small" { "Save updates this fragment in every loadout that uses it. Use " strong { "Save as a copy" } " to make a separate version under a new name." }
                        }
                    }
                    div class="modal-foot" {
                        @if !is_new {
                            button type="button" class="btn btn-danger delete-left"
                                hx-delete=(format!("/fragments/{}", enc(id))) hx-target="#main"
                                hx-confirm=(delete_confirm) {
                                (icon("trash")) "Delete"
                            }
                        }
                        button type="button" class="btn btn-ghost" hx-get="/close" hx-target="#modal" { "Cancel" }
                        @if !is_new {
                            button type="button" class="btn" hx-post="/fragments?as=copy" hx-target="#main" { (icon("copy")) "Save as a copy" }
                        }
                        button type="submit" class="btn btn-primary" { (icon("check")) "Save" }
                    }
                }
            }
        }
    }
    .into_string()
}

/// The output panel for a draft script "test run" (swapped into `#script-tryout`).
/// Shows stdout, an exit-code badge, and stderr when present — what the script
/// actually produces, so the user can confirm it works before saving.
pub fn script_tryout(out: &crate::providers::ProviderOutput) -> String {
    // `data` is Null only when the interpreter itself failed to spawn; then the
    // human-readable reason lives in `text`.
    let spawn_err = out.data.is_null();
    let stdout = out
        .data
        .get("stdout")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let stderr = out
        .data
        .get("stderr")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let status = out.data.get("status").and_then(|v| v.as_i64());
    html! {
        div class="tryout" {
            div class="tryout-head" {
                span class="tryout-label" { "Output" }
                @if spawn_err {
                    span class="tryout-status err" { "failed to run" }
                } @else if let Some(code) = status {
                    span class=(if code == 0 { "tryout-status ok" } else { "tryout-status err" }) {
                        "exit " (code)
                    }
                } @else {
                    span class="tryout-status err" { "killed" }
                }
            }
            @if spawn_err {
                pre class="tryout-body err" { (out.text) }
            } @else {
                @if stdout.is_empty() && stderr.is_empty() {
                    p class="tryout-empty muted small" { "(ran, no output)" }
                }
                @if !stdout.is_empty() { pre class="tryout-body" { (stdout) } }
                @if !stderr.is_empty() {
                    div class="tryout-stderr small muted" { "stderr" }
                    pre class="tryout-body err" { (stderr) }
                }
            }
        }
    }
    .into_string()
}

/// Shown when Run is clicked with an empty script.
pub fn script_tryout_empty() -> String {
    html! { p class="tryout-empty muted small" { "Nothing to run — the script is empty." } }
        .into_string()
}

fn close_btn() -> Markup {
    html! { button class="icon-btn" type="button" title="Close" aria-label="Close" hx-get="/close" hx-target="#modal" { (icon("x")) } }
}

/// Location control: a hidden, preserved `scope` (repo/global) + shared/private.
fn lives_in(layer: Layer) -> Markup {
    let (scope, private) = layer_scope(layer);
    html! {
        fieldset class="lives-in" {
            legend { "Where it lives" }
            input type="hidden" name="scope" value=(scope);
            div class="radio-row" {
                label class="radio" { input type="radio" name="visibility" value="public" checked[!private]; span { "shared" span class="radio-sub" { "config.toml" } } }
                label class="radio" { input type="radio" name="visibility" value="private" checked[private]; span { "private" span class="radio-sub" { "local.toml" } } }
            }
            @if scope == "global" { p class="hint small" { "Global — applies across all your repos." } }
        }
    }
}

// --- profile editor (full view) ----------------------------------------------

/// The full-width profile editor: a form (left) with name, targets, a fragment
/// picker, an inline quick-create, and a live preview (right). `draft` carries
/// the in-progress values (so an inline add re-renders without losing state).
pub fn profile_editor(
    draft: &LoadoutConfig,
    is_new: bool,
    original_name: Option<&str>,
    lib: &LibraryView,
    preview: &PreviewOutcome,
    error: Option<&str>,
) -> String {
    let name = draft.name.as_str();
    let selected: Vec<&str> = draft.fragments.iter().map(|r| r.id()).collect();
    let chosen = |id: &str| selected.contains(&id);
    // Target ids the catalog offers (built-ins + custom + machine); plus any id
    // the draft already references that the catalog doesn't know (a stale/typo'd
    // target) so editing never silently drops it.
    let known: std::collections::HashSet<&str> =
        lib.targets.iter().map(|t| t.id.as_str()).collect();
    let extra_targets: Vec<&str> = draft
        .targets
        .iter()
        .map(String::as_str)
        .filter(|t| !known.contains(t))
        .collect();
    html! {
        div class="profile-editor" {
            form class="editor-form" hx-post="/profiles/preview" hx-trigger="change delay:200ms" hx-target="#editor-preview" {
                @if !is_new { input type="hidden" name="new" value="0"; } @else { input type="hidden" name="new" value="1"; }
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { (if is_new { "New loadout" } else { "Edit loadout" }) }
                }
                @if let Some(err) = error {
                    div class="banner error" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { (err) } }
                }
                label class="field" { span class="field-label" { "name" span class="field-hint" { "required" } }
                    input type="text" name="name" value=(name) placeholder="rust — web" required;
                    // When editing, carry the original name as the rename key so
                    // the save can find-and-replace the right profile.
                    @if let Some(orig) = original_name { input type="hidden" name="original_name" value=(orig); }
                }
                fieldset class="targets-picker" {
                    legend { "Targets" span class="field-hint" { "applies when the repo looks like one of these" } }
                    div class="checks" {
                        @for t in &lib.targets {
                            label class="check" {
                                input type="checkbox" name="targets" value=(t.id.as_str()) checked[draft.targets.iter().any(|x| x == &t.id)];
                                span class="check-glyph" { (target_icon_markup(&resolve_target_icon(t.icon.as_deref(), &t.id))) }
                                span { (t.id.as_str()) }
                            }
                        }
                        @for &t in &extra_targets {
                            label class="check" {
                                input type="checkbox" name="targets" value=(t) checked;
                                span class="check-glyph" { (target_icon_markup(&resolve_target_icon(None, t))) }
                                span { (t) }
                            }
                        }
                    }
                }
                fieldset class="fragment-picker" {
                    legend { "Fragments" span class="field-hint" { "tick the ones to compose" } }
                    div class="pick-list" {
                        @for c in &lib.yours {
                            label class="pick" {
                                input type="checkbox" name="fragments" value=(c.id.as_str()) checked[chosen(c.id.as_str())];
                                span class="pick-glyph" { (icon(fragment_icon_name(c))) }
                                span class="pick-main" { span class="pick-title" { (c.title) } span class="pick-id" { (c.id.as_str()) } }
                            }
                        }
                    }
                    (inline_new_cap())
                }
                fieldset class="lives-in" {
                    legend { "Where it lives" }
                    p class="hint small" { "Global — every repo can use it; the loadout whose targets match a repo binds there." }
                    label class="check" { input type="checkbox" name="disabled" checked[draft.disabled]; span { "Disabled (kept, but never selected)" } }
                }
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/tab/profiles" hx-target="#main" { "Cancel" }
                    button type="button" class="btn btn-primary" hx-post="/profiles" hx-target="#main" { (icon("check")) "Stage loadout" }
                }
            }
            aside class="editor-preview-col" {
                div class="preview-head" { span class="preview-title" { (icon("eye")) "Live preview" } }
                div id="editor-preview" { (editor_preview(preview)) }
            }
        }
    }
    .into_string()
}

/// The collapsible inline "new fragment" mini-form inside the profile editor.
/// Its fields are `fragment_*`-namespaced so they don't collide with the profile form;
/// "Add" posts the whole editor form to `/profiles/draft`.
fn inline_new_cap() -> Markup {
    html! {
        details class="inline-cap" {
            summary { (icon("plus")) "New fragment" }
            div class="inline-grid" {
                label class="field" { span class="field-label" { "title" }
                    input type="text" name="fragment_name" placeholder="New fragment";
                }
                div class="seg seg-sm" {
                    input type="radio" name="fragment_kind" id="fragment-kind-md" value="markdown" checked;
                    label class="seg-opt" for="fragment-kind-md" { "Markdown" }
                    input type="radio" name="fragment_kind" id="fragment-kind-sc" value="script";
                    label class="seg-opt" for="fragment-kind-sc" { "Script" }
                }
                label class="field" { span class="field-label" { "content" }
                    textarea name="fragment_content" rows="3" placeholder="Guidance markdown, or the script body." {}
                }
                label class="check" { input type="checkbox" name="fragment_private"; span { "private (local.toml)" } }
                button type="button" class="btn btn-primary btn-sm" hx-post="/profiles/draft" hx-target="#main" { (icon("plus")) "Add to library & profile" }
            }
        }
    }
}

fn editor_preview(p: &PreviewOutcome) -> Markup {
    html! {
        div class="provenance" {
            span class="prov-node" { (p.context_summary.as_str()) }
            span class="prov-arrow" { (icon("arrow-right")) }
            span class="prov-node" { (p.fragment_count) " " (if p.fragment_count == 1 { "fragment" } else { "fragments" }) }
        }
        @if let Some(note) = &p.note { p class="note" { (note) } }
        div class="markdown-body" { (render_markdown(&p.overlay)) }
    }
}

pub fn editor_preview_fragment(p: &PreviewOutcome) -> String {
    editor_preview(p).into_string()
}

// --- diff / review -----------------------------------------------------------

pub fn diff_view(
    diffs: &[FileDiff],
    leaks: &[String],
    fs_changed: &[std::path::PathBuf],
    staged: usize,
) -> String {
    html! {
        div class="review" {
            div class="dash-head" {
                div class="editor-head" {
                    button type="button" class="icon-btn" title="Back" hx-get="/tab/profiles" hx-target="#main" { (icon("arrow-right")) }
                    h1 { "Review staged changes" }
                }
                span class="pill" { (staged) " staged" }
            }

            @if !leaks.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" { p { "Leak check: these public values look machine-specific — consider moving to local.toml:" } p class="mono" { (leaks.join(", ")) } }
                }
            } @else {
                p class="ok-line" { (icon("check")) "Leak check: clean." }
            }

            @if !fs_changed.is_empty() {
                div class="banner warn" {
                    span class="banner-icon" { (icon("alert")) }
                    div class="banner-body" { p { "Config changed on disk since load (" (fs_changed.iter().map(|p| display_name(p)).collect::<Vec<_>>().join(", ")) ") — applying will overwrite it." } }
                }
            }

            @if diffs.is_empty() {
                p class="empty" { "No staged changes." }
            } @else {
                @for d in diffs { (file_diff(d)) }
                div class="form-buttons" {
                    button type="button" class="btn btn-ghost" hx-get="/tab/profiles" hx-target="#main" { "Back" }
                    button type="button" class="btn btn-danger discard-left" hx-post="/discard" hx-target="#main"
                        hx-confirm="Discard all staged changes? Your config files won't be modified." { (icon("x")) "Discard all" }
                    button class="btn btn-primary" hx-post="/apply" hx-target="#main" hx-confirm="Apply staged changes to your config files?" { (icon("check")) "Apply " (staged) " change(s)" }
                }
            }
        }
    }
    .into_string()
}

fn file_diff(d: &FileDiff) -> Markup {
    let (scope, private) = layer_scope(d.layer);
    let vis = if private { "private" } else { "public" };
    html! {
        div class="file-diff" {
            div class="file-head" { span class="file-path" { (display_name(&d.path)) } span class="file-meta" { (scope) " · " (vis) } }
            @if d.reformats_untouched { p class="hint small" { "loadout will also reformat some untouched lines it parsed." } }
            pre class="diff-body" { (d.unified) }
        }
    }
}

// --- mutation results --------------------------------------------------------

/// A fragment mutation: re-render the Fragments tab into `#main`, close the
/// modal, and refresh the staged indicator. (`flash` keeps the "staged …" note.)
pub fn fragment_result(lib: &LibraryView, flash: &str) -> String {
    html! {
        (fragments_tab(lib, Some(flash)))
        (modal_close())
        (staged_refresh())
    }
    .into_string()
}

/// An inline error fragment (validation / config errors never 500).
pub fn error_fragment(msg: &str) -> String {
    html! { div class="banner error" { span class="banner-icon" { (icon("alert")) } div class="banner-body" { (msg) } } }.into_string()
}

/// A minimal full-page error (when the shell itself can't be assembled).
pub fn error_page(msg: &str) -> String {
    html! { (DOCTYPE) html { head { title { "Loadout studio — error" } } body { pre class="error" { (msg) } } } }.into_string()
}

// --- shared bits -------------------------------------------------------------

fn atom_dot(a: &AtomDot) -> Markup {
    let (cls, tip) = match a.state {
        AtomState::Owned => ("atom owned".to_string(), format!("{} — composed", a.id)),
        AtomState::Palette => (
            "atom palette".to_string(),
            format!("{} — palette only (not duplicated)", a.id),
        ),
        AtomState::Unknown => (
            "atom unknown".to_string(),
            format!("{} — unknown fragment", a.id),
        ),
    };
    html! { span class=(cls) title=(tip) {} }
}

fn layer_scope(layer: Layer) -> (&'static str, bool) {
    match layer {
        Layer::Global => ("global", false),
        Layer::GlobalLocal => ("global", true),
        Layer::RepoLocal => ("repo", true),
        _ => ("repo", false),
    }
}

fn display_name(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.display().to_string())
}

/// Percent-encode a path segment (profile names can contain spaces / em-dashes).
fn enc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cv(id: &str, category: Option<&str>) -> FragmentView {
        FragmentView {
            id: id.into(),
            title: id.into(),
            summary: None,
            kind: "static",
            category: category.map(str::to_string),
            script_lang: None,
            private: false,
        }
    }

    #[test]
    fn shell_has_capitalized_brand_and_theme_toggle() {
        let html = shell(maud::html! {}, 0, "fragments");
        // Wordmark + page title are capitalized; the lowercase command name is
        // not what the chrome shows.
        assert!(html.contains(r#"<span class="brand-name">Loadout</span>"#));
        assert!(html.contains("Loadout studio"));
        // Right-side controls are grouped so the nav tabs can center.
        assert!(html.contains(r#"class="topbar-right""#));
        // Theme toggle button with all three preference glyphs present.
        assert!(html.contains(r#"id="theme-toggle""#));
        assert!(html.contains("ti-auto"));
        assert!(html.contains("ti-light"));
        assert!(html.contains("ti-dark"));
    }

    #[test]
    fn shell_inlines_no_flash_theme_init() {
        let html = shell(maud::html! {}, 0, "fragments");
        // The inline head script must set the resolved theme + preference before
        // the stylesheet link, so there's no dark→light flash on load. (The
        // attribute is set at runtime via `dataset.theme`; it isn't in the SSR
        // markup, so assert on the script's own tokens instead.)
        assert!(html.contains("dataset.theme"));
        assert!(html.contains("prefers-color-scheme"));
        let init = html.find("loadout-theme").expect("theme init present");
        let css = html.find("studio.css").expect("stylesheet link present");
        assert!(
            init < css,
            "theme init must run before the stylesheet paints"
        );
    }

    #[test]
    fn fragments_group_in_friendly_order() {
        let caps = vec![
            cv("a", Some("comms")),
            cv("b", None),
            cv("c", Some("stack")),
            cv("d", Some("awareness")),
            cv("e", Some("stack")),
            cv("f", Some("zebra-custom")),
        ];
        let groups = group_fragments(&caps);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        // Known categories in CATEGORY_ORDER, then unknown categories (alpha),
        // then the uncategorized "General" bucket last.
        assert_eq!(
            labels,
            vec![
                "Awareness",
                "Stack conventions",
                "Communication",
                "Zebra custom",
                "General"
            ]
        );
        // A group keeps its members in library order.
        let stack = groups
            .iter()
            .find(|(l, _)| l == "Stack conventions")
            .unwrap();
        let ids: Vec<&str> = stack.1.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["c", "e"]);
    }

    #[test]
    fn category_label_titlecases_unknown_tags() {
        assert_eq!(category_label(Some("stack")), "Stack conventions");
        assert_eq!(category_label(Some("my-custom_tag")), "My custom tag");
        assert_eq!(category_label(None), "General");
    }

    #[test]
    fn friendly_categories_group_by_name_in_logical_order() {
        // The dedicated `category` field carries friendly names; they keep their
        // own label and sort in CATEGORY_ORDER before the legacy tag fallback.
        let caps = vec![
            cv("a", Some("Engineering Standards")),
            cv("b", Some("Operating Style")),
            cv("c", Some("Local Environment")),
        ];
        let groups = group_fragments(&caps);
        let labels: Vec<&str> = groups.iter().map(|(l, _)| l.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Operating Style",
                "Local Environment",
                "Engineering Standards"
            ]
        );
    }

    // --- target icons --------------------------------------------------------

    #[test]
    fn resolve_target_icon_picks_glyph_or_lettermark() {
        // A token naming a brand renders the filled brand logo.
        assert!(matches!(
            resolve_target_icon(Some("rust"), "rust"),
            TargetIcon::Logo(_)
        ));
        // A custom glyph pick (a general-purpose line glyph) renders as a glyph.
        assert!(matches!(
            resolve_target_icon(Some("database"), "deno"),
            TargetIcon::Glyph("database")
        ));
        // No token → a lettermark derived from the id.
        match resolve_target_icon(None, "deno") {
            TargetIcon::Mark(m) => assert_eq!(m, "DE"),
            _ => panic!("expected a lettermark"),
        }
        // A non-glyph token marks from its own text (the custom-letters escape).
        match resolve_target_icon(Some("k8s"), "kubernetes") {
            TargetIcon::Mark(m) => assert_eq!(m, "K8"),
            _ => panic!("expected a lettermark from the token"),
        }
    }

    #[test]
    fn target_chip_renders_glyph_and_lettermark() {
        // A built-in glyph target shows its SVG glyph (no lettermark badge).
        let rust = target_chip("rust", Some("rust")).into_string();
        assert!(rust.contains("<svg"), "glyph chip has an icon: {rust}");
        assert!(!rust.contains("lettermark"), "glyph chip has no badge");
        assert!(rust.contains("rust"));
        // A custom target with no icon shows a lettermark badge.
        let deno = target_chip("deno", None).into_string();
        assert!(deno.contains(r#"class="lettermark""#), "got: {deno}");
        assert!(deno.contains(">DE<"), "badge derived from id: {deno}");
    }

    #[test]
    fn target_dialog_offers_an_icon_picker() {
        // A new target's modal shows the picker with the lettermark (auto) tile
        // selected and the glyph tiles available.
        let html = target_dialog(None, Layer::Global);
        assert!(html.contains(r#"name="icon""#), "icon field present");
        assert!(
            html.contains(r#"id="ic-auto" value="" checked"#),
            "lettermark tile is the default: {html}"
        );
        assert!(html.contains(r#"id="ic-rust""#), "a glyph tile is offered");
        // Editing a target with a chosen glyph pre-selects that tile.
        let t = TargetDef {
            id: "deno".into(),
            description: None,
            icon: Some("database".into()),
            rule: TargetRule::FileExists {
                path: "deno.json".into(),
            },
            disabled: false,
            origin: Layer::Global,
        };
        let edit = target_dialog(Some(&t), Layer::Global);
        assert!(
            edit.contains(r#"id="ic-database" value="database" checked"#),
            "chosen glyph pre-selected: {edit}"
        );
        assert!(
            !edit.contains(r#"id="ic-auto" value="" checked"#),
            "auto tile not selected when a glyph is chosen"
        );
    }

    #[test]
    fn targets_tab_row_shows_the_target_glyph() {
        let view = TargetsView {
            targets: vec![TargetView {
                id: "rust".into(),
                description: Some("a Cargo manifest".into()),
                rule_summary: "Cargo.toml exists".into(),
                icon: Some("rust".into()),
                builtin: true,
                detected: false,
                is_script: false,
                editable: false,
                private: false,
            }],
        };
        let html = targets_tab(&view, None).into_string();
        assert!(html.contains("<svg"), "the row renders a glyph");
        assert!(html.contains("rust"));
    }
}
