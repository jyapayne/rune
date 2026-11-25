//! Linux and the BSDs: system font files found through fontconfig, in process (fontique's
//! system collection over the dlopened `libfontconfig`, which reads fontconfig's own cache), as
//! DirectWrite finds them on Windows (`direct_write`): no font folder is walked and no
//! `fc-match` or `fc-list` is started. A character the app's fonts lack goes to fontconfig's
//! match for its script and language, the font a GTK or Qt app falls back to; one in no script
//! of its own (a symbol, a private use icon) to the first font of fontconfig's `sans-serif`
//! fallback chain that has it, as Pango picks one.

use std::path::PathBuf;

use parley::fontique::{Blob, Collection, CollectionOptions, FallbackKey, FamilyId, GenericFamily, Language, Script, SourceKind};

thread_local! {
    static SYSTEM: std::cell::RefCell<Option<Collection>> = const { std::cell::RefCell::new(None) };
    static CHAIN: std::cell::RefCell<Option<Chain>> = const { std::cell::RefCell::new(None) };
}

fn with_system<R>(f: impl FnOnce(&mut Collection) -> R) -> R {
    SYSTEM.with_borrow_mut(|system| f(system.get_or_insert_with(|| Collection::new(CollectionOptions { shared: false, system_fonts: true }))))
}

/// A family of fontconfig's fallback chain and, once a character has needed it, its first
/// face's bytes (mapped, not read) and index; `None` when it has no file to read.
struct Link {
    family: FamilyId,
    face: Option<Option<(Blob<u8>, u32)>>,
}

impl Link {
    fn covers(&mut self, system: &mut Collection, ch: char) -> bool {
        let face = self.face.get_or_insert_with(|| {
            let family = system.family(self.family)?;
            let font = family.fonts().first()?;
            Some((font.load(None)?, font.index()))
        });
        let Some((blob, index)) = face else { return false };
        skrifa::FontRef::from_index(blob.as_ref(), *index).is_ok_and(|font| skrifa::MetadataProvider::charmap(&font).map(ch).is_some())
    }
}

/// The families tried first for a private-use character, as installed: Nerd Fonts' own icon
/// font, so an icon draws in the font made for it whatever font comes first in the chain.
const ICON_FAMILIES: &[&str] = &["Symbols Nerd Font", "Symbols Nerd Font Mono"];

/// Whether `ch` is a private-use codepoint (Nerd Font icons and other icon fonts).
fn is_private_use(ch: char) -> bool {
    matches!(ch as u32, 0xE000..=0xF8FF | 0xF0000..=0x10FFFF)
}

/// The families a character in no script of its own is looked up in: `ICON_FAMILIES` (for
/// private use characters only), then fontconfig's fallback chain.
struct Chain {
    icons: Vec<Link>,
    links: Vec<Link>,
}

/// fontconfig's fallback chain: the families of its sorted `sans-serif`, `monospace` and
/// `emoji` matches, each once, in order. Sorted with trimming, so a family is there only when
/// it has characters the ones before it lack, and every character an installed font has is in
/// some family of it.
fn chain(system: &mut Collection) -> Chain {
    let mut families: Vec<FamilyId> = Vec::new();
    for generic in [GenericFamily::SansSerif, GenericFamily::Monospace, GenericFamily::Emoji] {
        for id in system.generic_families(generic) {
            if !families.contains(&id) { families.push(id); }
        }
    }
    let icons = ICON_FAMILIES.iter().filter_map(|name| system.family_id(name)).map(|family| Link { family, face: None }).collect();
    Chain { icons, links: families.into_iter().map(|family| Link { family, face: None }).collect() }
}

/// The first family that has each of `chars` (`Chain`), each once.
fn covering_families(system: &mut Collection, chars: &[char]) -> Vec<FamilyId> {
    CHAIN.with_borrow_mut(|chain_| {
        let Chain { icons, links } = chain_.get_or_insert_with(|| chain(system));
        let mut out = Vec::new();
        for &ch in chars {
            let icons = if is_private_use(ch) { &mut icons[..] } else { &mut [][..] };
            if let Some(family) = icons.iter_mut().chain(links.iter_mut()).find_map(|link| link.covers(system, ch).then_some(link.family))
                && !out.contains(&family)
            {
                out.push(family);
            }
        }
        out
    })
}

/// Every file of family `id` in the system collection.
fn files_of(system: &mut Collection, id: FamilyId, out: &mut Vec<PathBuf>) {
    let Some(family) = system.family(id) else { return };
    for font in family.fonts() {
        if let SourceKind::Path(path) = &font.source().kind
            && !out.iter().any(|f| f.as_path() == path.as_ref())
        {
            out.push(path.to_path_buf());
        }
    }
}

/// The files of the installed families `names` (any of their names, case aside), in order.
pub(super) fn family_files(names: &[&str]) -> Vec<PathBuf> {
    with_system(|system| {
        let mut out = Vec::new();
        for name in names {
            if let Some(id) = system.family_id(name) { files_of(system, id, &mut out); }
        }
        out
    })
}

/// The family fontconfig resolves `monospace` to, and its files.
pub(super) fn monospace() -> Option<(String, Vec<PathBuf>)> {
    with_system(|system| {
        let id = system.generic_families(GenericFamily::Monospace).next()?;
        let name = system.family_name(id)?.to_owned();
        let mut files = Vec::new();
        files_of(system, id, &mut files);
        Some((name, files))
    })
}

/// The first colour emoji family fontconfig resolves `emoji` to, and its files.
pub(super) fn emoji() -> Option<(String, Vec<PathBuf>)> {
    with_system(|system| {
        let (id, name) = super::first_color_emoji_family(system)?;
        let mut files = Vec::new();
        files_of(system, id, &mut files);
        Some((name, files))
    })
}

/// The files of the fonts fontconfig matches for the scripts of `chars` in `locale`, and of
/// the fallback chain's first family with each character in no script of its own (symbols,
/// private use icons), each family whole (its bold and light faces too). A character no
/// installed font covers adds nothing.
pub(super) fn fallback_family_files(chars: &[char], locale: Option<&str>) -> Vec<PathBuf> {
    use icu_properties::{CodePointMapData, PropertyNamesShort, props::Script as UScript};
    let language = locale.and_then(|tag| Language::parse(tag).ok());
    let names = PropertyNamesShort::<UScript>::new();
    let mut scripts: Vec<[u8; 4]> = Vec::new();
    let mut unscripted: Vec<char> = Vec::new();
    for &ch in chars {
        let script = CodePointMapData::<UScript>::new().get(ch);
        if matches!(script, UScript::Common | UScript::Inherited | UScript::Unknown) {
            unscripted.push(ch);
            continue;
        }
        let Some(tag) = names.get_locale_script(script) else { continue };
        let Ok(bytes) = <[u8; 4]>::try_from(tag.as_str().as_bytes()) else { continue };
        if !scripts.contains(&bytes) { scripts.push(bytes); }
    }
    with_system(|system| {
        let mut out = Vec::new();
        for script in scripts {
            let ids: Vec<FamilyId> = system.fallback_families(FallbackKey::new(Script::from_bytes(script), language.as_ref())).take(2).collect();
            for id in ids { files_of(system, id, &mut out); }
        }
        if !unscripted.is_empty() {
            for id in covering_families(system, &unscripted) { files_of(system, id, &mut out); }
        }
        out
    })
}
