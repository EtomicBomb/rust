//! Rustdoc writes out two kinds of shared files:
//!  - Static files, which are embedded in the rustdoc binary and are written with a
//!    filename that includes a hash of their contents. These will always have a new
//!    URL if the contents change, so they are safe to cache with the
//!    `Cache-Control: immutable` directive. They are written under the static.files/
//!    directory and are written when --emit-type is empty (default) or contains
//!    "toolchain-specific". If using the --static-root-path flag, it should point
//!    to a URL path prefix where each of these filenames can be fetched.
//!  - Invocation specific files. These are generated based on the crate(s) being
//!    documented. Their filenames need to be predictable without knowing their
//!    contents, so they do not include a hash in their filename and are not safe to
//!    cache with `Cache-Control: immutable`. They include the contents of the
//!    --resource-suffix flag and are emitted when --emit-type is empty (default)
//!    or contains "invocation-specific".

use std:: marker::PhantomData;
use std::cell::RefCell;
use std::{fs, io, fmt};
use std::io::Write as _;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::ffi::OsString;
use std::collections::hash_map::Entry;
use std::iter::once;
use std::str::FromStr;
use std::any::Any;

use indexmap::IndexMap;
use itertools::Itertools;
use rustc_data_structures::flock;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::ty::fast_reject::{DeepRejectCtxt, TreatParams};
use rustc_middle::ty::TyCtxt;
use rustc_span::def_id::DefId;
use rustc_span::Symbol;
use serde::ser::SerializeSeq;
use serde::{Serialize, Deserialize, de::DeserializeOwned, Serializer};
use regex::Regex;

use super::{collect_paths_for_type, ensure_trailing_slash, Context, RenderMode};
use crate::html::render::search_index::build_index;
use crate::html::render::sorted_json::SortedJson;
use crate::html::render::sorted_template::{self, SortedTemplate};
use crate::clean::{Crate, Item, ItemId, ItemKind};
use crate::config::{EmitType, RenderOptions, PathToParts};
use crate::docfs::PathError;
use crate::error::Error;
use crate::formats::cache::Cache;
use crate::formats::item_type::ItemType;
use crate::formats::Impl;
use crate::html::format::Buffer;
use crate::html::render::search_index::SerializedSearchIndex;
use crate::html::render::{AssocItemLink, ImplRenderingParameters};
use crate::html::layout;
use crate::html::static_files::{self, suffix_path};
use crate::visit::DocVisitor;
use crate::{try_err, try_none};

// TODO: string faster loading tetchnique with JSON.stringify

/// Write crate-info.json cross-crate information, static files, invocation-specific files, etc. to disk
pub(crate) fn write_shared(
    cx: &mut Context<'_>,
    krate: &Crate,
    opt: &RenderOptions,
    tcx: TyCtxt<'_>,
) -> Result<(), Error> {
    // NOTE(EtomicBomb): I don't think we need sync here because no read-after-write?
    Rc::get_mut(&mut cx.shared).unwrap().fs.set_sync_only(true);
    let lock_file = cx.dst.join(".lock");
    // Write shared runs within a flock; disable thread dispatching of IO temporarily.
    let _lock = try_err!(flock::Lock::new(&lock_file, true, true, true), &lock_file);

    let crate_name = krate.name(cx.tcx());
    let crate_name = crate_name.as_str(); // rand
    let crate_name_json = SortedJson::serialize(crate_name); // "rand"
    let SerializedSearchIndex { index, desc } = build_index(&krate, &mut Rc::get_mut(&mut cx.shared).unwrap().cache, tcx);
    let external_crates = hack_get_external_crate_names(cx)?;
    let info = CrateInfo {
        src_files_js: PartsAndLocations::<SourcesPart>::get(cx, &crate_name_json)?,
        search_index_js: PartsAndLocations::<SearchIndexPart>::get(cx, index)?,
        all_crates: PartsAndLocations::<AllCratesPart>::get(crate_name_json.clone())?,
        crates_index: PartsAndLocations::<CratesIndexPart>::get(&crate_name, &external_crates)?,
        trait_impl: PartsAndLocations::<TraitAliasPart>::get(cx, &crate_name_json)?,
        type_impl: PartsAndLocations::<TypeAliasPart>::get(cx, krate, &crate_name_json)?,
    };

    if let Some(parts_out_dir) = &opt.parts_out_dir {
        let path = parts_out_dir.0.clone();
        write_create_parents(cx, dbg!(path), serde_json::to_string(&info).unwrap())?;
    }

    let mut crates_info = CrateInfo::read(&opt.parts_paths)?;
    crates_info.push(info);

    if opt.write_rendered_cci {
        write_static_files(cx, &opt)?;
        write_search_desc(cx, &krate, &desc)?;
        if opt.emit.is_empty() || opt.emit.contains(&EmitType::InvocationSpecific) {
            if cx.include_sources {
                write_rendered_cci::<SourcesPart>(cx, opt.read_rendered_cci, &crates_info)?;
            }
            write_rendered_cci::<SearchIndexPart>(cx, opt.read_rendered_cci, &crates_info)?;
            write_rendered_cci::<AllCratesPart>(cx, opt.read_rendered_cci, &crates_info)?;
        }
        write_rendered_cci::<TraitAliasPart>(cx, opt.read_rendered_cci, &crates_info)?;
        write_rendered_cci::<TypeAliasPart>(cx, opt.read_rendered_cci, &crates_info)?;
        match &opt.index_page {
            Some(index_page) if opt.enable_index_page => {
                let mut md_opts = opt.clone();
                md_opts.output = cx.dst.clone();
                md_opts.external_html = cx.shared.layout.external_html.clone();
                try_err!(crate::markdown::render(&index_page, md_opts, cx.shared.edition()), &index_page);
            }
            None if opt.enable_index_page => {
                write_rendered_cci::<CratesIndexPart>(cx, opt.read_rendered_cci, &crates_info)?;
            }
            _ => {}, // they don't want an index page
        }
    }

    Rc::get_mut(&mut cx.shared).unwrap().fs.set_sync_only(false);
    Ok(())
}

/// Writes the static files, the style files, and the css extensions
fn write_static_files(
    cx: &mut Context<'_>,
    options: &RenderOptions,
) -> Result<(), Error> {
    let static_dir = cx.dst.join("static.files");

    cx.shared
        .fs
        .create_dir_all(&static_dir)
        .map_err(|e| PathError::new(e, "static.files"))?;

    // Handle added third-party themes
    for entry in &cx.shared.style_files {
        let theme = entry.basename()?;
        let extension =
            try_none!(try_none!(entry.path.extension(), &entry.path).to_str(), &entry.path);

        // Skip the official themes. They are written below as part of STATIC_FILES_LIST.
        if matches!(theme.as_str(), "light" | "dark" | "ayu") {
            continue;
        }

        let bytes = try_err!(fs::read(&entry.path), &entry.path);
        let filename = format!("{theme}{suffix}.{extension}", suffix = cx.shared.resource_suffix);
        cx.shared.fs.write(cx.dst.join(filename), bytes)?;
    }

    // When the user adds their own CSS files with --extend-css, we write that as an
    // invocation-specific file (that is, with a resource suffix).
    if let Some(ref css) = cx.shared.layout.css_file_extension {
        let buffer = try_err!(fs::read_to_string(css), css);
        let path = static_files::suffix_path("theme.css", &cx.shared.resource_suffix);
        cx.shared.fs.write(cx.dst.join(path), buffer)?;
    }

    if options.emit.is_empty() || options.emit.contains(&EmitType::Toolchain) {
        static_files::for_each(|f: &static_files::StaticFile| {
            let filename = static_dir.join(f.output_filename());
            cx.shared.fs.write(filename, f.minified())
        })?;
    }

    Ok(())
}

/// Write the search description shards to disk
fn write_search_desc(cx: &mut Context<'_>, krate: &Crate, search_desc: &[(usize, String)]) -> Result<(), Error> {
    let crate_name = krate.name(cx.tcx()).to_string();
    let encoded_crate_name = SortedJson::serialize(&crate_name);
    let path = PathBuf::from_iter([&cx.dst, Path::new("search.desc"), Path::new(&crate_name)]);
    if Path::new(&path).exists() {
        try_err!(fs::remove_dir_all(&path), &path);
    }
    for (i, (_, part)) in search_desc.iter().enumerate() {
        let filename = static_files::suffix_path(
            &format!("{crate_name}-desc-{i}-.js"),
            &cx.shared.resource_suffix,
        );
        let path = path.join(filename);
        let part = SortedJson::serialize(&part);
        let part = format!("searchState.loadedDescShard({encoded_crate_name}, {i}, {part})");
        write_create_parents(cx, path, part)?;
    }
    Ok(())
}

/// Written to `crate-info.json`. Contains pre-rendered contents to insert into the CCI template
#[derive(Serialize, Deserialize, Clone, Debug)]
struct CrateInfo {
    src_files_js: PartsAndLocations<SourcesPart>,
    search_index_js: PartsAndLocations<SearchIndexPart>,
    all_crates: PartsAndLocations<AllCratesPart>,
    crates_index: PartsAndLocations<CratesIndexPart>,
    trait_impl: PartsAndLocations<TraitAliasPart>,
    type_impl: PartsAndLocations<TypeAliasPart>,
}

impl CrateInfo {
    /// Gets a reference to the cross-crate information parts for `T`
    fn get<T: 'static>(&self) -> Option<&PartsAndLocations<T>> {
        (&self.src_files_js as &dyn Any).downcast_ref()
            .or_else(|| (&self.search_index_js as &dyn Any).downcast_ref())
            .or_else(|| (&self.all_crates as &dyn Any).downcast_ref())
            .or_else(|| (&self.crates_index as &dyn Any).downcast_ref())
            .or_else(|| (&self.trait_impl as &dyn Any).downcast_ref())
            .or_else(|| (&self.type_impl as &dyn Any).downcast_ref())
    }

    /// read all of the crate info from its location on the filesystem
    fn read(parts_paths: &[PathToParts]) -> Result<Vec<Self>, Error> {
        parts_paths.iter()
            .map(|parts_path| {
                let path = &parts_path.0;
                let parts = try_err!(fs::read(&path), &path);
                let parts: CrateInfo = try_err!(serde_json::from_slice(&parts), &path);
                Ok::<_, Error>(parts)
            })
            .collect::<Result<Vec<CrateInfo>, Error>>()
    }
}

/// Paths (relative to the doc root) and their pre-merge contents
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(transparent)]
struct PartsAndLocations<P> {
    parts: Vec<(PathBuf, P)>,
}

impl<P> Default for PartsAndLocations<P> {
    fn default() -> Self {
        Self { parts: Vec::default() }
    }
}

impl<T, U> PartsAndLocations<Part<T, U>> {
    fn push(&mut self, path: PathBuf, item: U) {
        self.parts.push((path, Part { _artifact: PhantomData, item }));
    }

    /// Singleton part, one file
    fn with(path: PathBuf, part: U) -> Self {
        let mut ret = Self::default();
        ret.push(path, part);
        ret
    }
}

/// A piece of one of the shared artifacts for documentation (search index, sources, alias list, etc.)
///
/// Merged at a user specified time and written to the `doc/` directory
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(transparent)]
struct Part<T, U> {
    #[serde(skip)]
    _artifact: PhantomData<T>,
    item: U,
}

impl<T, U: fmt::Display> fmt::Display for Part<T, U> {
    /// Writes serialized JSON
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.item)
    }
}

/// Wrapper trait for `Part<T, U>`
pub(crate) trait CciPart: Sized + fmt::Display + 'static {
    /// Identifies the kind of cross crate information.
    ///
    /// The cci type name in `doc.parts/<cci type>`
    type FileFormat: sorted_template::FileFormat;
    fn blank_template(cx: &Context<'_>) -> SortedTemplate<Self::FileFormat>;
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct SearchIndex;
type SearchIndexPart = Part<SearchIndex, SortedJson>;
impl CciPart for SearchIndexPart {
    type FileFormat = sorted_template::Js;
    fn blank_template(_cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        SortedTemplate::before_after(r"var searchIndex = new Map([", r"]);
if (typeof exports !== 'undefined') exports.searchIndex = searchIndex;
else if (window.initSearch) window.initSearch(searchIndex);")
    }
}
impl PartsAndLocations<SearchIndexPart> {
    fn get(cx: &Context<'_>, search_index: SortedJson) -> Result<Self, Error> {
        let path = suffix_path("search-index.js", &cx.shared.resource_suffix);
        Ok(Self::with(path, search_index))
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct AllCrates;
type AllCratesPart = Part<AllCrates, SortedJson>;
impl CciPart for AllCratesPart {
    type FileFormat = sorted_template::Js;
    fn blank_template(_cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        SortedTemplate::before_after("window.ALL_CRATES = [", "];")
    }
}
impl PartsAndLocations<AllCratesPart> {
    fn get(crate_name_json: SortedJson) -> Result<Self, Error> {
        let path = PathBuf::from("crates.js");
        Ok(Self::with(path, crate_name_json))
    }
}
/// Reads `crates.js`, which seems like the best
/// place to obtain the list of externally documented crates if the index
/// page was disabled when documenting the deps
fn hack_get_external_crate_names(cx: &Context<'_>) -> Result<Vec<String>, Error> {
    let path = cx.dst.join("crates.js");
    let Ok(content) = fs::read_to_string(&path) else {
        // they didn't emit invocation specific, so we just say there were no crates
        return Ok(Vec::default());
    };
    // this is only run once so it's fine not to cache it
    // dot_matches_new_line false: all crates on same line. greedy: match last bracket
    let regex = Regex::new(r"\[.*\]").unwrap();
    let Some(content) = regex.find(&content) else {
        return Err(Error::new("could not find crates list in crates.js", path));
    };
    let content: Vec<String> = try_err!(serde_json::from_str(content.as_str()), &path);
    Ok(content)
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct CratesIndex;
type CratesIndexPart = Part<CratesIndex, String>;
impl CciPart for CratesIndexPart {
    type FileFormat = sorted_template::Html;
    fn blank_template(cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        let mut magic = String::from("\u{FFFC}");
        let page = layout::Page {
            title: "Index of crates",
            css_class: "mod sys",
            root_path: "./",
            static_root_path: cx.shared.static_root_path.as_deref(),
            description: "List of crates",
            resource_suffix: &cx.shared.resource_suffix,
            rust_logo: true,
        };
        let layout = &cx.shared.layout;
        let style_files = &cx.shared.style_files;
        // HACK(EtomicBomb): This is fine
        loop {
            let content = format!("<h1>List of all crates</h1><ul class=\"all-items\">{magic}</ul>");
            let template = layout::render(layout, &page, "", content, &style_files);
            match SortedTemplate::magic(&template, &magic) {
                Ok(template) => return template,
                Err(_) => magic.push_str("\u{FFFC}"),
            }
        }
    }
}
impl PartsAndLocations<CratesIndexPart> {
    /// Might return parts that are duplicate with ones in prexisting index.html
    fn get(crate_name: &str, external_crates: &[String]) -> Result<Self, Error> {
        let mut ret = Self::default();
        let path = PathBuf::from("index.html");
        for crate_name in external_crates.iter().map(|s| s.as_str()).chain(once(crate_name)) {
            let part = format!(
                "<li><a href=\"{trailing_slash}index.html\">{crate_name}</a></li>",
                trailing_slash = ensure_trailing_slash(crate_name),
            );
            ret.push(path.clone(), part);
        }
        Ok(ret)
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct Sources;
type SourcesPart = Part<Sources, SortedJson>;
impl CciPart for SourcesPart {
    type FileFormat = sorted_template::Js;
    fn blank_template(_cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        // This needs to be `var`, not `const`.
        // This variable needs declared in the current global scope so that if
        // src-script.js loads first, it can pick it up.
        SortedTemplate::before_after(r"var srcIndex = new Map([", r"]);
createSrcSidebar();")
    }
}
impl PartsAndLocations<SourcesPart> {
    fn get(cx: &Context<'_>, crate_name: &SortedJson) -> Result<Self, Error> {
        let hierarchy = Rc::new(Hierarchy::default());
        cx
            .shared
            .local_sources
            .iter()
            .filter_map(|p| p.0.strip_prefix(&cx.shared.src_root).ok())
            .for_each(|source| hierarchy.add_path(source));
        let path = suffix_path("src-files.js", &cx.shared.resource_suffix);
        let hierarchy = hierarchy.to_json_string();
        let part = SortedJson::array_unsorted([crate_name, &hierarchy]);
        Ok(Self::with(path, part))
    }
}

/// Source files directory tree
#[derive(Debug, Default)]
struct Hierarchy {
    parent: Weak<Self>,
    elem: OsString,
    children: RefCell<FxHashMap<OsString, Rc<Self>>>,
    elems: RefCell<FxHashSet<OsString>>,
}

impl Hierarchy {
    fn with_parent(elem: OsString, parent: &Rc<Self>) -> Self {
        Self { elem, parent: Rc::downgrade(parent), ..Self::default() }
    }

    fn to_json_string(&self) -> SortedJson {
        let subs = self.children.borrow();
        let files = self.elems.borrow();
        let name = SortedJson::serialize(self.elem.to_str().expect("invalid osstring conversion"));
        let mut out = Vec::from([name]);
        if !subs.is_empty() || !files.is_empty() {
            let subs = subs.iter().map(|(_, s)| s.to_json_string());
            out.push(SortedJson::array(subs));
        }
        if !files.is_empty() {
            let files = files.iter().map(|s| SortedJson::serialize(s.to_str().expect("invalid osstring")));
            out.push(SortedJson::array(files));
        }
        SortedJson::array_unsorted(out)
    }

    fn add_path(self: &Rc<Self>, path: &Path) {
        let mut h = Rc::clone(&self);
        let mut elems = path
            .components()
            .filter_map(|s| match s {
                Component::Normal(s) => Some(s.to_owned()),
                Component::ParentDir => Some(OsString::from("..")),
                _ => None,
            })
            .peekable();
        loop {
            let cur_elem = elems.next().expect("empty file path");
            if cur_elem == ".." {
                if let Some(parent) = h.parent.upgrade() {
                    h = parent;
                }
                continue;
            }
            if elems.peek().is_none() {
                h.elems.borrow_mut().insert(cur_elem);
                break;
            } else {
                let entry = Rc::clone(
                    h.children
                        .borrow_mut()
                        .entry(cur_elem.clone())
                        .or_insert_with(|| Rc::new(Self::with_parent(cur_elem, &h))),
                );
                h = entry;
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct TypeAlias;
type TypeAliasPart = Part<TypeAlias, SortedJson>;
impl CciPart for TypeAliasPart {
    type FileFormat = sorted_template::Js;
    fn blank_template(_cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        SortedTemplate::before_after(r"(function() {
    var type_impls = Object.fromEntries([", r"]);
    if (window.register_type_impls) {
        window.register_type_impls(type_impls);
    } else {
        window.pending_type_impls = type_impls;
    }
})()")
    }
}

impl PartsAndLocations<TypeAliasPart> {
    fn get(cx: &mut Context<'_>, krate: &Crate, crate_name_json: &SortedJson) -> Result<Self, Error> {
        let cache = &Rc::clone(&cx.shared).cache;
        let mut path_parts = Self::default();

        let mut type_impl_collector = TypeImplCollector {
            aliased_types: IndexMap::default(),
            visited_aliases: FxHashSet::default(),
            cache,
            cx,
        };
        DocVisitor::visit_crate(&mut type_impl_collector, &krate);
        let cx = type_impl_collector.cx;
        let aliased_types = type_impl_collector.aliased_types;
        for aliased_type in aliased_types.values() {
            let impls = aliased_type
                .impl_
                .values()
                .flat_map(|AliasedTypeImpl { impl_, type_aliases }| {
                    let mut ret = Vec::new();
                    let trait_ = impl_
                        .inner_impl()
                        .trait_
                        .as_ref()
                        .map(|trait_| format!("{:#}", trait_.print(cx)));
                    // render_impl will filter out "impossible-to-call" methods
                    // to make that functionality work here, it needs to be called with
                    // each type alias, and if it gives a different result, split the impl
                    for &(type_alias_fqp, ref type_alias_item) in type_aliases {
                        let mut buf = Buffer::html();
                        cx.id_map = Default::default();
                        cx.deref_id_map = Default::default();
                        let target_did = impl_
                            .inner_impl()
                            .trait_
                            .as_ref()
                            .map(|trait_| trait_.def_id())
                            .or_else(|| impl_.inner_impl().for_.def_id(cache));
                        let provided_methods;
                        let assoc_link = if let Some(target_did) = target_did {
                            provided_methods = impl_.inner_impl().provided_trait_methods(cx.tcx());
                            AssocItemLink::GotoSource(ItemId::DefId(target_did), &provided_methods)
                        } else {
                            AssocItemLink::Anchor(None)
                        };
                        super::render_impl(
                            &mut buf,
                            cx,
                            *impl_,
                            &type_alias_item,
                            assoc_link,
                            RenderMode::Normal,
                            None,
                            &[],
                            ImplRenderingParameters {
                                show_def_docs: true,
                                show_default_items: true,
                                show_non_assoc_items: true,
                                toggle_open_by_default: true,
                            },
                        );
                        let text = buf.into_inner();
                        let type_alias_fqp = (*type_alias_fqp).iter().join("::");
                        if Some(&text) == ret.last().map(|s: &AliasSerializableImpl| &s.text) {
                            ret.last_mut()
                                .expect("already established that ret.last() is Some()")
                                .aliases
                                .push(type_alias_fqp);
                        } else {
                            ret.push(AliasSerializableImpl {
                                text,
                                trait_: trait_.clone(),
                                aliases: vec![type_alias_fqp],
                            })
                        }
                    }
                    ret
                })
                .collect::<Vec<_>>();

            let mut path = PathBuf::from("type.impl");
            for component in &aliased_type.target_fqp[..aliased_type.target_fqp.len() - 1] {
                path.push(component.as_str());
            }
            let aliased_item_type = aliased_type.target_type;
            path.push(&format!(
                "{aliased_item_type}.{}.js",
                aliased_type.target_fqp[aliased_type.target_fqp.len() - 1]
            ));

            let part = SortedJson::array(impls.iter().map(SortedJson::serialize).collect::<Vec<_>>());
            path_parts.push(path, SortedJson::array_unsorted([crate_name_json, &part]));
        }
        Ok(path_parts)
    }
}


#[derive(Serialize, Deserialize, Clone, Default, Debug)]
struct TraitAlias;
type TraitAliasPart = Part<TraitAlias, SortedJson>;
impl CciPart for TraitAliasPart {
    type FileFormat = sorted_template::Js;
    fn blank_template(_cx: &Context<'_>) -> SortedTemplate<Self::FileFormat> {
        SortedTemplate::before_after(r"(function() {
    var implementors = Object.fromEntries([", r"]);
    if (window.register_implementors) {
        window.register_implementors(implementors);
    } else {
        window.pending_implementors = implementors;
    }
})()")
    }
}
impl PartsAndLocations<TraitAliasPart> {
    fn get(cx: &mut Context<'_>, crate_name_json: &SortedJson) -> Result<Self, Error> {
        let cache = &cx.shared.cache;
        let mut path_parts = Self::default();
        // Update the list of all implementors for traits
        // <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+trait.impl&type=code>
        for (&did, imps) in &cache.implementors {
            // Private modules can leak through to this phase of rustdoc, which
            // could contain implementations for otherwise private types. In some
            // rare cases we could find an implementation for an item which wasn't
            // indexed, so we just skip this step in that case.
            //
            // FIXME: this is a vague explanation for why this can't be a `get`, in
            //        theory it should be...
            let (remote_path, remote_item_type) = match cache.exact_paths.get(&did) {
                Some(p) => match cache.paths.get(&did).or_else(|| cache.external_paths.get(&did)) {
                    Some((_, t)) => (p, t),
                    None => continue,
                },
                None => match cache.external_paths.get(&did) {
                    Some((p, t)) => (p, t),
                    None => continue,
                },
            };

            let implementors = imps
                .iter()
                .filter_map(|imp| {
                    // If the trait and implementation are in the same crate, then
                    // there's no need to emit information about it (there's inlining
                    // going on). If they're in different crates then the crate defining
                    // the trait will be interested in our implementation.
                    //
                    // If the implementation is from another crate then that crate
                    // should add it.
                    if imp.impl_item.item_id.krate() == did.krate || !imp.impl_item.item_id.is_local() {
                        None
                    } else {
                        Some(Implementor {
                            text: imp.inner_impl().print(false, cx).to_string(),
                            synthetic: imp.inner_impl().kind.is_auto(),
                            types: collect_paths_for_type(imp.inner_impl().for_.clone(), cache),
                        })
                    }
                })
                .collect::<Vec<_>>();

            // Only create a js file if we have impls to add to it. If the trait is
            // documented locally though we always create the file to avoid dead
            // links.
            if implementors.is_empty() && !cache.paths.contains_key(&did) {
                continue;
            }

            let mut path = PathBuf::from("trait.impl");
            for component in &remote_path[..remote_path.len() - 1] {
                path.push(component.as_str());
            }
            path.push(&format!("{remote_item_type}.{}.js", remote_path[remote_path.len() - 1]));

            let part = SortedJson::array(implementors.iter().map(SortedJson::serialize).collect::<Vec<_>>());
            path_parts.push(path, SortedJson::array_unsorted([crate_name_json, &part]));
        }
        Ok(path_parts)
    }
}

struct Implementor {
    text: String,
    synthetic: bool,
    types: Vec<String>,
}

impl Serialize for Implementor {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&self.text)?;
        if self.synthetic {
            seq.serialize_element(&1)?;
            seq.serialize_element(&self.types)?;
        }
        seq.end()
    }
}

/// Collect the list of aliased types and their aliases.
/// <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+type.impl&type=code>
///
/// The clean AST has type aliases that point at their types, but
/// this visitor works to reverse that: `aliased_types` is a map
/// from target to the aliases that reference it, and each one
/// will generate one file.
struct TypeImplCollector<'cx, 'cache> {
    /// Map from DefId-of-aliased-type to its data.
    aliased_types: IndexMap<DefId, AliasedType<'cache>>,
    visited_aliases: FxHashSet<DefId>,
    cache: &'cache Cache,
    cx: &'cache mut Context<'cx>,
}

/// Data for an aliased type.
///
/// In the final file, the format will be roughly:
///
/// ```json
/// // type.impl/CRATE/TYPENAME.js
/// JSONP(
/// "CRATE": [
///   ["IMPL1 HTML", "ALIAS1", "ALIAS2", ...],
///   ["IMPL2 HTML", "ALIAS3", "ALIAS4", ...],
///    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ struct AliasedType
///   ...
/// ]
/// )
/// ```
struct AliasedType<'cache> {
    /// This is used to generate the actual filename of this aliased type.
    target_fqp: &'cache [Symbol],
    target_type: ItemType,
    /// This is the data stored inside the file.
    /// ItemId is used to deduplicate impls.
    impl_: IndexMap<ItemId, AliasedTypeImpl<'cache>>,
}

/// The `impl_` contains data that's used to figure out if an alias will work,
/// and to generate the HTML at the end.
///
/// The `type_aliases` list is built up with each type alias that matches.
struct AliasedTypeImpl<'cache> {
    impl_: &'cache Impl,
    type_aliases: Vec<(&'cache [Symbol], Item)>,
}

impl<'cx, 'cache> DocVisitor for TypeImplCollector<'cx, 'cache> {
    fn visit_item(&mut self, it: &Item) {
        self.visit_item_recur(it);
        let cache = self.cache;
        let ItemKind::TypeAliasItem(ref t) = *it.kind else { return };
        let Some(self_did) = it.item_id.as_def_id() else { return };
        if !self.visited_aliases.insert(self_did) {
            return;
        }
        let Some(target_did) = t.type_.def_id(cache) else { return };
        let get_extern = { || cache.external_paths.get(&target_did) };
        let Some(&(ref target_fqp, target_type)) =
            cache.paths.get(&target_did).or_else(get_extern)
        else {
            return;
        };
        let aliased_type = self.aliased_types.entry(target_did).or_insert_with(|| {
            let impl_ = cache
                .impls
                .get(&target_did)
                .map(|v| &v[..])
                .unwrap_or_default()
                .iter()
                .map(|impl_| {
                    (
                        impl_.impl_item.item_id,
                        AliasedTypeImpl { impl_, type_aliases: Vec::new() },
                    )
                })
                .collect();
            AliasedType { target_fqp: &target_fqp[..], target_type, impl_ }
        });
        let get_local = { || cache.paths.get(&self_did).map(|(p, _)| p) };
        let Some(self_fqp) = cache.exact_paths.get(&self_did).or_else(get_local) else {
            return;
        };
        let aliased_ty = self.cx.tcx().type_of(self_did).skip_binder();
        // Exclude impls that are directly on this type. They're already in the HTML.
        // Some inlining scenarios can cause there to be two versions of the same
        // impl: one on the type alias and one on the underlying target type.
        let mut seen_impls: FxHashSet<ItemId> = cache
            .impls
            .get(&self_did)
            .map(|s| &s[..])
            .unwrap_or_default()
            .iter()
            .map(|i| i.impl_item.item_id)
            .collect();
        for (impl_item_id, aliased_type_impl) in &mut aliased_type.impl_ {
            // Only include this impl if it actually unifies with this alias.
            // Synthetic impls are not included; those are also included in the HTML.
            //
            // FIXME(lazy_type_alias): Once the feature is complete or stable, rewrite this
            // to use type unification.
            // Be aware of `tests/rustdoc/type-alias/deeply-nested-112515.rs` which might regress.
            let Some(impl_did) = impl_item_id.as_def_id() else { continue };
            let for_ty = self.cx.tcx().type_of(impl_did).skip_binder();
            let reject_cx =
                DeepRejectCtxt { treat_obligation_params: TreatParams::AsCandidateKey };
            if !reject_cx.types_may_unify(aliased_ty, for_ty) {
                continue;
            }
            // Avoid duplicates
            if !seen_impls.insert(*impl_item_id) {
                continue;
            }
            // This impl was not found in the set of rejected impls
            aliased_type_impl.type_aliases.push((&self_fqp[..], it.clone()));
        }
    }
}

/// Final serialized form of the alias impl
struct AliasSerializableImpl {
    text: String,
    trait_: Option<String>,
    aliases: Vec<String>,
}

impl Serialize for AliasSerializableImpl {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = serializer.serialize_seq(None)?;
        seq.serialize_element(&self.text)?;
        if let Some(trait_) = &self.trait_ {
            seq.serialize_element(trait_)?;
        } else {
            seq.serialize_element(&0)?;
        }
        for type_ in &self.aliases {
            seq.serialize_element(type_)?;
        }
        seq.end()
    }
}

/// Create all parents
fn create_parents(cx: &mut Context<'_>, path: &Path) -> Result<(), Error> {
    let parent = path.parent().expect("trying to write to an empty path");
    // TODO: check cache for whether this directory has already been created
    try_err!(cx.shared.fs.create_dir_all(parent), parent);
    Ok(())
}

/// Create parents and then write
fn write_create_parents(cx: &mut Context<'_>, path: PathBuf, content: String) -> Result<(), Error> {
    create_parents(cx, &path)?;
    cx.shared.fs.write(path, content)?;
    Ok(())
}

/// info from this crate and the --include-info-json'd crates
fn write_rendered_cci<T: CciPart + DeserializeOwned>(
    cx: &mut Context<'_>,
    read_rendered_cci: bool,
    crates_info: &[CrateInfo],
) -> Result<(), Error> {
    // read parts from disk
    let path_parts = crates_info.iter()
        .map(|crate_info| crate_info.get::<T>().unwrap().parts.iter())
        .flatten();
    // read previous rendered cci from storage, append to them
    let mut templates: FxHashMap<PathBuf, SortedTemplate<T::FileFormat>> = Default::default();
    for (path, part) in path_parts {
        let part = format!("{part}");
        let path = cx.dst.join(&path);
        match templates.entry(path.clone()) {
            Entry::Vacant(entry) => {
                let template = entry.insert(if read_rendered_cci {
                    match fs::read_to_string(&path) {
                        Ok(template) => try_err!(SortedTemplate::from_str(&template), &path),
                        Err(e) if e.kind() == io::ErrorKind::NotFound => T::blank_template(cx),
                        Err(e) => return Err(Error::new(e, &path)),
                    }
                } else {
                    T::blank_template(cx)
                });
                template.append(part);
            }
            Entry::Occupied(mut t) => t.get_mut().append(part),
        }
    }
    // write the merged cci to disk
    for (path, template) in templates {
        create_parents(cx, &path)?;
        let file = try_err!(File::create(&path), &path);
        let mut file = BufWriter::new(file);
        try_err!(write!(file, "{template}"), &path);
        try_err!(file.flush(), &path);
    }
    Ok(())
}

