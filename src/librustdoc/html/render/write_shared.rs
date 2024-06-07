#![allow(dead_code)]

use std::cell::RefCell;
use std::fs::{self, read_dir};
use std::path::{Component, Path, PathBuf};
use std::rc::{Rc, Weak};
use std::fmt::{self, Display};
use std::ops::Deref;
use std::ffi::OsString;


use indexmap::IndexMap;
use itertools::Itertools;
use rustc_data_structures::flock;
use rustc_data_structures::fx::{FxHashMap, FxHashSet};
use rustc_middle::ty::fast_reject::{DeepRejectCtxt, TreatParams};
use rustc_span::def_id::DefId;
use rustc_span::Symbol;
use serde::ser::SerializeSeq;
use serde::{Serialize, Deserialize, Serializer};

use super::{collect_paths_for_type, ensure_trailing_slash, Context, SharedContext, RenderMode};
use crate::clean::{Crate, Item, ItemId, ItemKind};
use crate::config::{EmitType, RenderOptions};
use crate::docfs::PathError;
use crate::error::Error;
use crate::formats::cache::Cache;
use crate::formats::item_type::ItemType;
use crate::formats::Impl;
use crate::html::format::Buffer;
use crate::html::render::search_index::SerializedSearchIndex;
use crate::html::render::{AssocItemLink, ImplRenderingParameters};
use crate::html::{layout, static_files};
use crate::visit::DocVisitor;
use crate::{try_err, try_none};

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

    fn to_json_string(&self) -> SortedJson<String> {
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
        SortedJson::array(out)
    }

    // subs empty, files empty - [name]
    // subs empty, files full - [name, subs, files]
    // subs full, files empty - [name, subs]
    // subs full, files full - [name, subs, files]

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


#[derive(Clone, Debug, PartialOrd, Ord, PartialEq, Eq, Serialize, Deserialize)]
struct SortedJson<T>(T);

impl<T: Deref> SortedJson<T> {
    fn as_deref(&self) -> SortedJson<&<T as Deref>::Target> {
        SortedJson(self.0.deref())
    }
}

impl<T: Display> Display for SortedJson<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl SortedJson<String> {
    fn serialize<T: Serialize>(item: T) -> Self {
        SortedJson(serde_json::to_string(&item).unwrap())
    }

    fn array<T: Display + Ord, I: IntoIterator<Item=SortedJson<T>>>(items: I) -> Self {
        let mut items: Vec<_> = items.into_iter().collect();
        items.sort_unstable();
        SortedJson(format!("[{}]", items.into_iter().format(",")))
    }

    fn object<K: Display, V: Display, I: IntoIterator<Item=(K, SortedJson<V>)>>(items: I) -> Self {
        let mut items: Vec<String> = items.into_iter().map(|(k, v)| format!("{k}:{v}")).collect();
        items.sort_unstable();
        SortedJson(format!("{{{}}}", items.into_iter().format(",")))
    }
}


#[allow(dead_code)]
trait Snip: Serialize + Default + for <'a> Deserialize<'a> {
    const NAME: &'static str;
    fn merge(&mut self, other: Self);
    /// It is very annoying that this takes the shared context. It is only used in the crate list,
    /// because it renders an entire page based off of whatever the layout and style files are
    /// present.
    fn render(&self, shared: &SharedContext<'_>) -> String;
}

struct InvocationIdentifier(String);

fn snip_dst(dst: &Path, invocation: &InvocationIdentifier) -> PathBuf {
    [dst, Path::new("snips"), Path::new(&invocation.0)].into_iter().collect()
}

fn merge_snips<S: Snip>(dst: &Path) -> Result<S, Error> {
    let dst = dst.join("snips");
    let snips = try_err!(read_dir(&dst), &dst)
        .map(|snip_path| {
            let snip_path = try_err!(snip_path, &dst); // TODO: better error
            let snip_path = snip_path.path();
            let snip = try_err!(fs::read(&snip_path), &snip_path);
            let snip = try_err!(serde_json::from_slice(&snip), &snip_path);
            Ok::<S, Error>(snip)
        })
        .try_fold(S::default(), |mut snips, snip| {
            snips.merge(snip?);
            Ok(snips)
        })?;
    Ok(snips)
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct SourcesSnip {
    sources: Vec<SortedJson<String>>,
}

impl SourcesSnip {
    fn new(crate_name: SortedJson<&str>, hierarchy: &Hierarchy) -> SourcesSnip {
        let hierarchy = hierarchy.to_json_string();
        let sources = Vec::from([SortedJson::array([crate_name, hierarchy.as_deref()])]);
        SourcesSnip { sources }
    }
}

impl Snip for SourcesSnip {
    const NAME: &'static str = "sources";
    fn merge(&mut self, mut other: Self) {
        self.sources.append(&mut other.sources);
    }

    /// Render search-index.js
    fn render(&self, _shared: &SharedContext<'_>) -> String {
        let sources = SortedJson::array(self.sources.iter().map(|e| e.as_deref()));
        // This needs to be `var`, not `const`.
        // This variable needs declared in the current global scope so that if
        // src-script.js loads first, it can pick it up.
        format!("var srcIndex = new Map({sources}); createSrcSidebar();")
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct SearchIndexSnip {
    all_indexes: Vec<SortedJson<String>>,
}

impl SearchIndexSnip {
    fn new(index: &str) -> Self {
        Self { all_indexes: Vec::from([SortedJson::serialize(index)]) }
    }
}

impl Snip for SearchIndexSnip {
    const NAME: &'static str = "search-index";
    fn merge(&mut self, mut other: Self) {
        self.all_indexes.append(&mut other.all_indexes);
    }

    /// Render search-index.js
    fn render(&self, _shared: &SharedContext<'_>) -> String {
        let all_indexes = SortedJson::array(self.all_indexes.iter().map(|e| e.as_deref()));
        format!(r"#\
var searchIndex = new Map(JSON.parse('{all_indexes}'));
if (typeof exports !== 'undefined') exports.searchIndex = searchIndex;
else if (window.initSearch) window.initSearch(searchIndex);
#")
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct SearchDescriptionSnip {
    descs: Vec<SortedJson<String>>,
}

impl Snip for SearchDescriptionSnip {
    const NAME: &'static str = "search-index";
    fn merge(&mut self, mut other: Self) {
        self.descs.append(&mut other.descs);
    }

    /// render a search description shard
    fn render(&self, _shared: &SharedContext<'_>) -> String {
        todo!() 
    }
}

fn render(snip: &SearchDescriptionSnip, shard: usize, crate_name: SortedJson<&str>) -> String {
    let data = SortedJson::array(snip.descs.iter().map(|e| e.as_deref()));
    format!("searchState.loadedDescShard({crate_name}, {shard}, {data})")
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct AllCratesSnip {
    crates: Vec<String>,
}

impl AllCratesSnip {
    fn new(crate_name: String) -> AllCratesSnip {
        AllCratesSnip { crates: Vec::from([crate_name]) }
    }
}

impl Snip for AllCratesSnip {
    const NAME: &'static str = "all-crates";
    fn merge(&mut self, mut other: Self) {
        self.crates.append(&mut other.crates);
    }

    fn render(&self, _shared: &SharedContext<'_>) -> String {
        let crates = SortedJson::array(self.crates.iter().map(|krate| SortedJson::serialize(krate)).collect::<Vec<_>>());
        format!("window.ALL_CRATES = {crates};")
    }
}

impl AllCratesSnip {
    fn render_crates_index(&self, shared: &SharedContext<'_>) -> String {
        let page = layout::Page {
            title: "Index of crates",
            css_class: "mod sys",
            root_path: "./",
            static_root_path: shared.static_root_path.as_deref(),
            description: "List of crates",
            resource_suffix: &shared.resource_suffix,
            rust_logo: true,
        };
        let layout = &shared.layout;
        let style_files = &shared.style_files;
        let content = format!(
            "<h1>List of all crates</h1><ul class=\"all-items\">{}</ul>",
            self.crates.iter().format_with("", |k, f| {
                f(&format_args!(
                    "<li><a href=\"{trailing_slash}index.html\">{k}</a></li>",
                    trailing_slash = ensure_trailing_slash(k),
                ))
            })
        );
        layout::render(layout, &page, "", content, &style_files)
    }
}

fn implementors_iife(impls: &str, register: &str, pending: &str, json: SortedJson<&str>) -> String {
    format!(r"#
(function() {{
    var {impls} = {json};
    if (window.{register}) {{
        window.{register}({impls});
    }} else {{
        window.{pending} = {impls};
    }}
}})();
#")
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct TypeAliasSnip {
    aliases: Vec<(SortedJson<String>, SortedJson<String>)>,
}

impl Snip for TypeAliasSnip {
    const NAME: &'static str = "type-alias";
    fn merge(&mut self, mut other: Self) {
        self.aliases.append(&mut other.aliases);
    }

    fn render(&self, _shared: &SharedContext<'_>) -> String {
        let aliases = SortedJson::object(self.aliases.iter().map(|(k, v)| (k.as_deref(), v.as_deref())));
        implementors_iife("type_impls", "register_type_impls", "pending_type_impls", aliases.as_deref())
    }
}

#[derive(Serialize, Deserialize, Default, Debug)]
struct TraitAliasSnip {
    implementors: Vec<(SortedJson<String>, SortedJson<String>)>,
}

impl Snip for TraitAliasSnip {
    const NAME: &'static str = "trait-alias";
    fn merge(&mut self, mut other: Self) {
        self.implementors.append(&mut other.implementors);
    }

    fn render(&self, _shared: &SharedContext<'_>) -> String {
        let implementors = SortedJson::object(self.implementors.iter().map(|(k, v)| (k.as_deref(), v.as_deref())));
        implementors_iife("implementors", "register_implementors", "pending_type_impls", implementors.as_deref())
    }
}

pub(super) fn write_shared(
    cx: &mut Context<'_>,
    krate: &Crate,
    search_index: SerializedSearchIndex,
    options: &RenderOptions,
) -> Result<(), Error> {
    dump(cx, krate, search_index, options)
}

/// Rustdoc writes out two kinds of shared files:
///  - Static files, which are embedded in the rustdoc binary and are written with a
///    filename that includes a hash of their contents. These will always have a new
///    URL if the contents change, so they are safe to cache with the
///    `Cache-Control: immutable` directive. They are written under the static.files/
///    directory and are written when --emit-type is empty (default) or contains
///    "toolchain-specific". If using the --static-root-path flag, it should point
///    to a URL path prefix where each of these filenames can be fetched.
///  - Invocation specific files. These are generated based on the crate(s) being
///    documented. Their filenames need to be predictable without knowing their
///    contents, so they do not include a hash in their filename and are not safe to
///    cache with `Cache-Control: immutable`. They include the contents of the
///    --resource-suffix flag and are emitted when --emit-type is empty (default)
///    or contains "invocation-specific".
pub(super) fn dump(
    cx: &mut Context<'_>,
    krate: &Crate,
    search_index: SerializedSearchIndex,
    options: &RenderOptions,
) -> Result<(), Error> {
    // Write out the shared files. Note that these are shared among all rustdoc
    // docs placed in the output directory, so this needs to be a synchronized
    // operation with respect to all other rustdocs running around.

    let crate_name = krate.name(cx.tcx()).to_string();
    let encoded_crate_name = SortedJson::serialize(crate_name.clone());
    let invocation = InvocationIdentifier(crate_name.clone());

    let lock_file = cx.dst.join(".lock");
    let _lock = try_err!(flock::Lock::new(&lock_file, true, true, true), &lock_file);

    // InvocationSpecific resources should always be dynamic.
    let write_invocation_specific = |p: &str, make_content: &dyn Fn() -> Result<Vec<u8>, Error>| {
        let content = make_content()?;
        if options.emit.is_empty() || options.emit.contains(&EmitType::InvocationSpecific) {
            let output_filename = static_files::suffix_path(p, &cx.shared.resource_suffix);
            cx.shared.fs.write(cx.dst.join(output_filename), content)
        } else {
            Ok(())
        }
    };

    cx.shared
        .fs
        .create_dir_all(cx.dst.join("static.files"))
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
        let static_dir = cx.dst.join(Path::new("static.files"));
        static_files::for_each(|f: &static_files::StaticFile| {
            let filename = static_dir.join(f.output_filename());
            cx.shared.fs.write(filename, f.minified())
        })?;
    }

    if cx.include_sources {
        let hierarchy = Rc::new(Hierarchy::default());
        for source in cx
            .shared
            .local_sources
            .iter()
            .filter_map(|p| p.0.strip_prefix(&cx.shared.src_root).ok())
        {
            hierarchy.add_path(source);
        }

        
        let dst = cx.dst.join("src-files.js"); // TODO: invocation specific
        let sources = SourcesSnip::new(encoded_crate_name.as_deref(), &hierarchy);
        cx.shared.fs.write(snip_dst(&dst, &invocation), serde_json::to_string(&sources).unwrap())?;
        let sources = merge_snips::<SourcesSnip>(&dst)?; 
        write_invocation_specific("src-files.js", &|| {
            Ok(sources.render(&cx.shared).into_bytes())
        })?;
    }

    // TODO: append to tree instead of creating a subdirectory

    // Update the search index and crate list.
    let SerializedSearchIndex { index, desc } = search_index;

    let dst = cx.dst.join("search-index.js");
    let index = SearchIndexSnip::new(&index);
    cx.shared.fs.write(snip_dst(&dst, &invocation), serde_json::to_string(&index).unwrap())?;
    let index = merge_snips::<SearchIndexSnip>(&dst)?; 
    write_invocation_specific("search-index.js", &|| {
        Ok(index.render(&cx.shared).into_bytes())
    })?;

    // NOTE: this is fine because it odesn't write to a shared directory
    let search_desc_dir = cx.dst.join(format!("search.desc/{krate}", krate = krate.name(cx.tcx())));
    if Path::new(&search_desc_dir).exists() {
        try_err!(std::fs::remove_dir_all(&search_desc_dir), &search_desc_dir);
    }
    try_err!(std::fs::create_dir_all(&search_desc_dir), &search_desc_dir);
    let kratename = krate.name(cx.tcx()).to_string();
    for (i, (_, data)) in desc.into_iter().enumerate() {
        let output_filename = static_files::suffix_path(
            &format!("{kratename}-desc-{i}-.js"),
            &cx.shared.resource_suffix,
        );
        let path = search_desc_dir.join(output_filename);
        try_err!(
            std::fs::write(
                &path,
                &format!(
                    r##"searchState.loadedDescShard({encoded_crate_name}, {i}, {data})"##,
                    data = serde_json::to_string(&data).unwrap(),
                )
                .into_bytes()
            ),
            &path
        );
    }

    let dst_index = cx.dst.join("index.html");
    let all_crates = AllCratesSnip::new(crate_name.clone());
    cx.shared.fs.write(snip_dst(&dst_index, &invocation), serde_json::to_string(&all_crates).unwrap())?;
    let all_crates = merge_snips::<AllCratesSnip>(&dst_index)?; // TODO: snip_dst needs to append a file
    // TODO: need write and merge to avoid reading our own writess
    // TODO: consider how to clean up these files

    if options.enable_index_page {
        if let Some(index_page) = options.index_page.clone() {
            let mut md_opts = options.clone();
            md_opts.output = cx.dst.clone();
            md_opts.external_html = (*cx.shared).layout.external_html.clone();

            crate::markdown::render(&index_page, md_opts, cx.shared.edition())
                .map_err(|e| Error::new(e, &index_page))?;
        } else {
            let crates_index = all_crates.render_crates_index(&cx.shared);
            cx.shared.fs.write(dst_index, crates_index)?;
        }
    }

    write_invocation_specific("crates.js", &|| {
        Ok(all_crates.render(&cx.shared).into())
    })?;

    let cloned_shared = Rc::clone(&cx.shared);
    let cache = &cloned_shared.cache;

    // Collect the list of aliased types and their aliases.
    // <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+type.impl&type=code>
    //
    // The clean AST has type aliases that point at their types, but
    // this visitor works to reverse that: `aliased_types` is a map
    // from target to the aliases that reference it, and each one
    // will generate one file.

    struct TypeImplCollector<'cx, 'cache> {
        // Map from DefId-of-aliased-type to its data.
        aliased_types: IndexMap<DefId, AliasedType<'cache>>,
        visited_aliases: FxHashSet<DefId>,
        cache: &'cache Cache,
        cx: &'cache mut Context<'cx>,
    }

    // Data for an aliased type.
    //
    // In the final file, the format will be roughly:
    //
    // ```json
    // // type.impl/CRATE/TYPENAME.js
    // JSONP(
    // "CRATE": [
    //   ["IMPL1 HTML", "ALIAS1", "ALIAS2", ...],
    //   ["IMPL2 HTML", "ALIAS3", "ALIAS4", ...],
    //    ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ struct AliasedType
    //   ...
    // ]
    // )
    // ```

    struct AliasedType<'cache> {
        // This is used to generate the actual filename of this aliased type.
        target_fqp: &'cache [Symbol],
        target_type: ItemType,
        // This is the data stored inside the file.
        // ItemId is used to deduplicate impls.
        impl_: IndexMap<ItemId, AliasedTypeImpl<'cache>>,
    }

    // The `impl_` contains data that's used to figure out if an alias will work,
    // and to generate the HTML at the end.
    //
    // The `type_aliases` list is built up with each type alias that matches.

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

    let mut type_impl_collector = TypeImplCollector {
        aliased_types: IndexMap::default(),
        visited_aliases: FxHashSet::default(),
        cache,
        cx,
    };

    DocVisitor::visit_crate(&mut type_impl_collector, &krate);
    // Final serialized form of the alias impl
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

    let cx = type_impl_collector.cx;
    let dst = cx.dst.join("type.impl");
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

        // FIXME: this fixes only rustdoc part of instability of trait impls
        // for js files, see #120371
        // Manually collect to string and sort to make list not depend on order
        let impls = SortedJson::array(impls.iter().map(SortedJson::serialize).collect::<Vec<_>>());
        let impls = SortedJson::object([(encoded_crate_name.as_deref(), impls)]);

        let mut mydst = dst.clone();
        for part in &aliased_type.target_fqp[..aliased_type.target_fqp.len() - 1] {
            mydst.push(part.to_string());
        }
        cx.shared.ensure_dir(&mydst)?;
        let aliased_item_type = aliased_type.target_type;

        mydst.push(&format!(
            "{aliased_item_type}.{}.js",
            aliased_type.target_fqp[aliased_type.target_fqp.len() - 1]
        ));

        cx.shared.fs.write(snip_dst(&mydst, &invocation), serde_json::to_string(&impls).unwrap())?;
        let all_impls = merge_snips::<TypeAliasSnip>(&mydst)?;
        cx.shared.fs.write(mydst, all_impls.render(&cx.shared))?;
    }

    // Update the list of all implementors for traits
    // <https://github.com/search?q=repo%3Arust-lang%2Frust+[RUSTDOCIMPL]+trait.impl&type=code>
    let dst = cx.dst.join("trait.impl");
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

        // FIXME: this fixes only rustdoc part of instability of trait impls
        // for js files, see #120371
        // Manually collect to string and sort to make list not depend on order
        let implementors = SortedJson::array(implementors.iter().map(SortedJson::serialize).collect::<Vec<_>>());
        let implementors = SortedJson::object([(encoded_crate_name.as_deref(), implementors)]);

        let mut mydst = dst.clone();
        for part in &remote_path[..remote_path.len() - 1] {
            mydst.push(part.to_string());
        }
        cx.shared.ensure_dir(&mydst)?;
        mydst.push(&format!("{remote_item_type}.{}.js", remote_path[remote_path.len() - 1]));

        cx.shared.fs.write(snip_dst(&mydst, &invocation), serde_json::to_string(&implementors).unwrap())?;
        let all_implementors = merge_snips::<TraitAliasSnip>(&mydst)?;
        cx.shared.fs.write(mydst, all_implementors.render(&cx.shared))?;
    }
    Ok(())
}
