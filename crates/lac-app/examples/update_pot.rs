//! Writes `crates/lac-app/po/messages.pot`, the template of all texts
//! marked for translation (see `lac_app::i18n::extract`).
//!
//! ```sh
//! cargo run -p lac-app --example update_pot
//! msgmerge --update --backup=none crates/lac-app/po/de.po crates/lac-app/po/messages.pot
//! ```

use std::path::Path;

fn main() -> std::io::Result<()> {
    let app = Path::new(env!("CARGO_MANIFEST_DIR"));
    let messages = lac_app::i18n::extract::messages(&app.join(".."))?;
    let out = app.join("po/messages.pot");
    std::fs::write(&out, lac_app::i18n::extract::template(&messages))?;
    println!("{} texts in {}", messages.len(), out.display());
    Ok(())
}
