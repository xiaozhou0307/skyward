use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

pub mod modprobe;

/// Where a module source's files are in the recovery: the vendor_boot
/// ramdisk's modules under /lib/modules, the vendor_dlkm and vendor
/// partitions where they are mounted, and the kernel's own modules in
/// system_dlkm, which it also keeps in one folder.
pub fn source_dir(source: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(match source {
        "vendor_boot" => "/lib/modules",
        "vendor_dlkm" => "/vendor_dlkm/lib/modules",
        "vendor" => "/vendor/lib/modules",
        "odm_dlkm" => "/odm_dlkm/lib/modules",
        "system_dlkm" => "/system_dlkm/flatten/lib/modules",
        _ => bail!("unknown module source {source:?}"),
    }))
}

/// Load `names` (module names without `.ko`) with their dependencies. Each
/// comes from the first of `dirs` that holds it, and its dependencies from
/// that directory's modules.dep; a module the kernel already has counts as
/// loaded. Returns the files loaded, dependencies first.
pub fn load(dirs: &[PathBuf], names: &[String]) -> Result<Vec<PathBuf>> {
    let mut loaded = Vec::new();
    load_together(dirs, names, &mut loaded, &mut |why| Err(anyhow!("{why}")))?;
    Ok(loaded)
}

/// [`load`] for a place that would rather go on with what it could load,
/// as a phone's own start does: whatever could not be, `note` says.
pub fn load_relaxed(dirs: &[PathBuf], names: &[String], note: &mut dyn FnMut(String)) -> Vec<PathBuf> {
    let mut loaded = Vec::new();
    let mut strict = |why: String| {
        note(why);
        Ok::<(), anyhow::Error>(())
    };
    let _ = load_together(dirs, names, &mut loaded, &mut strict);
    loaded
}

/// Load the modules named, and what they need, from wherever the phone keeps
/// them: a kernel's modules are spread over several places, and the
/// `modules.dep` of one names files another holds, so every directory's graph
/// is read and each file is looked for in all of them.
fn load_together(
    dirs: &[PathBuf],
    names: &[String],
    loaded: &mut Vec<PathBuf>,
    note: &mut dyn FnMut(String) -> Result<()>,
) -> Result<()> {
    let graphs: Vec<ModuleGraph> = dirs.iter().filter_map(|d| parse_dep(d).ok()).collect();
    let mut done: BTreeSet<PathBuf> = loaded.iter().cloned().collect();
    for name in names {
        let file = format!("{name}.ko");
        if hold(dirs, &file).is_none() {
            note(format!("{file} is in none of {dirs:?}"))?;
            continue;
        }
        for m in order_across(&graphs, &file) {
            let places = copies(dirs, &m);
            if places.is_empty() {
                note(format!("{m} is named by a modules.dep, and no source holds it"))?;
                continue;
            }
            // A phone may keep the same module in more than one place, and
            // the kernel refuses one of them while it takes the other: the
            // copy on the ramdisk is not the copy on a partition.
            let mut done_here = false;
            let mut tried = Vec::new();
            for path in places {
                if done_here || done.contains(&path) {
                    done_here = true;
                    continue;
                }
                match insmod(&path, "") {
                    Ok(()) => {
                        done.insert(path.clone());
                        loaded.push(path);
                        done_here = true;
                    }
                    Err(e) => {
                        note(format!("could not load {}: {e:#}", path.display()))?;
                        tried.push(path);
                    }
                }
            }
            let _ = tried;
        }
    }
    Ok(())
}

/// Every place these hold `file`, in the order given: a module kept on more
/// than one of them is tried at each until one is taken.
pub fn copies(dirs: &[PathBuf], file: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs {
        let direct = dir.join(file);
        if direct.is_file() {
            found.push(direct);
        } else if let Some(deep) = find_file(dir, file) {
            found.push(deep);
        }
    }
    found
}

/// The file `file` as the phone holds it, in any of these places.
fn hold(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter().map(|d| d.join(file)).find(|p| p.is_file()).or_else(|| dirs.iter().find_map(|d| find_file(d, file)))
}

/// The order a module's dependencies come in, gathered from every graph: one
/// graph names the stack, another the driver that sits on it.
fn order_across(graphs: &[ModuleGraph], file: &str) -> Vec<String> {
    // One graph of them all: a module named by one place and kept by another
    // is a dependency like any other, and only a single walk over everything
    // both places say puts every module after what it needs.
    let mut deps: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for graph in graphs {
        for (module, needs) in &graph.deps {
            let known = deps.entry(module.clone()).or_default();
            for need in needs {
                if !known.contains(need) {
                    known.push(need.clone());
                }
            }
        }
    }
    ModuleGraph { base: PathBuf::new(), deps }.load_order(&[file.to_string()])
}

/// The symbols the kernel said it could not resolve, as it names them in its
/// own log: `cfg80211: Unknown symbol rfkill_register (err -2)`.
pub fn unknown_symbols(log: &str) -> BTreeSet<String> {
    let mut symbols = BTreeSet::new();
    for line in log.lines() {
        let Some((_, rest)) = line.split_once("Unknown symbol ") else {
            continue;
        };
        if let Some(symbol) = rest.split_whitespace().next() {
            symbols.insert(symbol.to_string());
        }
    }
    symbols
}

/// The modules that hold a symbol the kernel could not resolve, and which
/// symbol each holds: what one module needs of another is a name in the
/// other's strings, so the places the phone keeps its modules are searched
/// for it. A symbol no module holds is the kernel's own.
pub fn providing(dirs: &[PathBuf], symbols: &BTreeSet<String>) -> Vec<(PathBuf, String)> {
    /// A module larger than this is not the one a symbol of a module this
    /// size was missing, and reading a partition of modules whole for one
    /// name is what once held a page still for a minute.
    const LARGEST: u64 = 8 << 20;
    /// How many modules are offered for one symbol: the one named after it
    /// first, and a couple of others that mention it.
    const ENOUGH: usize = 3;
    let mut found: Vec<(PathBuf, String)> = Vec::new();
    for dir in dirs {
        for path in module_files(dir) {
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.len() > LARGEST {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            for symbol in symbols {
                if holds(&bytes, symbol.as_bytes()) {
                    found.push((path.clone(), symbol.clone()));
                }
            }
        }
    }
    // The module a symbol is named after is the one that defines it.
    let named = |path: &Path, symbol: &str| {
        let head = symbol.split('_').next().unwrap_or(symbol);
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        stem.contains(head)
    };
    found.sort_by_key(|(path, symbol)| !named(path, symbol));
    let mut chosen: Vec<(PathBuf, String)> = Vec::new();
    for (path, symbol) in found {
        let offered = chosen.iter().filter(|(_, s)| *s == symbol).count();
        if offered < ENOUGH {
            chosen.push((path, symbol));
        }
    }
    chosen
}

/// Whether `bytes` holds `symbol` as itself: modules keep the names of the
/// symbols they define in their strings.
fn holds(bytes: &[u8], symbol: &[u8]) -> bool {
    !symbol.is_empty() && bytes.windows(symbol.len()).any(|w| w == symbol)
}

/// Every module file under `dir`, its subdirectories included.
pub fn module_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            files.extend(module_files(&path));
        } else if path.extension().is_some_and(|e| e == "ko") {
            files.push(path);
        }
    }
    files
}

/// A `modules.dep` graph: each module file mapped to the module files it
/// depends on. Keys and values are reduced to bare file names so the graph is
/// independent of where the modules were sourced from (GKI devices keep them
/// in several places: vendor_boot ramdisk, vendor_dlkm, vendor).
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub base: PathBuf,
    pub deps: BTreeMap<String, Vec<String>>,
}

pub fn parse_dep(base: impl AsRef<Path>) -> Result<ModuleGraph> {
    let base = base.as_ref().to_path_buf();
    let dep_path = base.join("modules.dep");
    let text =
        fs::read_to_string(&dep_path).with_context(|| format!("read {}", dep_path.display()))?;
    let mut deps = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((lhs, rhs)) = line.split_once(':') else {
            continue;
        };
        let name = basename(lhs.trim());
        let ds = rhs.split_whitespace().map(basename).collect();
        deps.insert(name, ds);
    }
    Ok(ModuleGraph { base, deps })
}

fn basename(p: &str) -> String {
    Path::new(p)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| p.to_string())
}

impl ModuleGraph {
    /// Topologically ordered load list. With an empty `requested`, every module
    /// in the graph is scheduled; otherwise only the requested modules and
    /// their transitive dependencies, dependencies first.
    pub fn load_order(&self, requested: &[String]) -> Vec<String> {
        let mut order = Vec::new();
        let mut seen = BTreeSet::new();
        let list: Vec<String> = if requested.is_empty() {
            self.deps.keys().cloned().collect()
        } else {
            requested.to_vec()
        };
        for m in list {
            self.visit(&m, &mut seen, &mut order);
        }
        order
    }

    fn visit(&self, m: &str, seen: &mut BTreeSet<String>, order: &mut Vec<String>) {
        if !seen.insert(m.to_string()) {
            return;
        }
        if let Some(ds) = self.deps.get(m) {
            for d in ds {
                self.visit(d, seen, order);
            }
        }
        order.push(m.to_string());
    }

    pub fn resolve_path(&self, module: &str) -> Option<PathBuf> {
        let direct = self.base.join(module);
        if direct.exists() {
            return Some(direct);
        }
        find_file(&self.base, module)
    }
}

fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let rd = fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_file(&p, name) {
                return Some(f);
            }
        } else if p.file_name().map(|s| s == name).unwrap_or(false) {
            return Some(p);
        }
    }
    None
}

/// Hand the open module `file` to the kernel with the settings `params`.
#[cfg(target_os = "android")]
fn finit_module(file: &fs::File, params: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    let params = std::ffi::CString::new(params)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe {
        libc::syscall(
            libc::SYS_finit_module,
            file.as_raw_fd(),
            params.as_ptr(),
            0 as libc::c_int,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn finit_module(_file: &fs::File, _params: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "modules are loaded only on the phone",
    ))
}

/// Hand a module to the kernel as an image in memory, which some builds take
/// where they refuse the same bytes as a file.
#[cfg(target_os = "android")]
fn init_module(image: &[u8], params: &str) -> std::io::Result<()> {
    let params = std::ffi::CString::new(params)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let rc = unsafe { libc::syscall(libc::SYS_init_module, image.as_ptr().cast::<libc::c_void>(), image.len(), params.as_ptr()) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "android"))]
fn init_module(_image: &[u8], _params: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "modules are loaded only on the phone"))
}

pub fn insmod(path: impl AsRef<Path>, params: &str) -> Result<()> {
    let path = path.as_ref();
    let f = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    match finit_module(&f, params) {
        Ok(()) => Ok(()),
        // Already loaded is success for our purposes.
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
        // A kernel that refuses the file may still take the same bytes from
        // memory: some builds judge the way a module arrives rather than the
        // module, and the two ways are the only two there are.
        Err(first) => {
            let image = fs::read(path).with_context(|| format!("read {}", path.display()))?;
            match init_module(&image, params) {
                Ok(()) => Ok(()),
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => Ok(()),
                Err(second) => Err(first)
                    .with_context(|| format!("finit_module {}", path.display()))
                    .with_context(|| format!("init_module of the same bytes: {second}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_dependencies_first() {
        let mut deps = BTreeMap::new();
        deps.insert("a.ko".to_string(), vec!["b.ko".to_string()]);
        deps.insert("b.ko".to_string(), vec!["c.ko".to_string()]);
        deps.insert("c.ko".to_string(), vec![]);
        let g = ModuleGraph {
            base: PathBuf::new(),
            deps,
        };
        let order = g.load_order(&["a.ko".to_string()]);
        assert_eq!(order, vec!["c.ko", "b.ko", "a.ko"]);
    }

    #[test]
    fn strips_paths_to_basenames() {
        assert_eq!(basename("kernel/drivers/foo/bar.ko"), "bar.ko");
    }
}

#[cfg(test)]
mod split_tree_tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skyward-modules-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_dependency_the_other_place_holds_is_still_its_dependency() {
        // One place keeps the stack, the other the driver that sits on it,
        // and each names what it needs of the other.
        let one = scratch("one");
        let two = scratch("two");
        fs::write(one.join("rfkill.ko"), b"").unwrap();
        fs::write(one.join("cfg80211.ko"), b"").unwrap();
        fs::write(one.join("modules.dep"), "rfkill.ko:\ncfg80211.ko: rfkill.ko\n").unwrap();
        fs::write(two.join("wlan_drv.ko"), b"").unwrap();
        fs::write(two.join("modules.dep"), "wlan_drv.ko: cfg80211.ko\n").unwrap();
        let order = order_across(&[parse_dep(&one).unwrap(), parse_dep(&two).unwrap()], "wlan_drv.ko");
        assert_eq!(order, ["rfkill.ko", "cfg80211.ko", "wlan_drv.ko"], "dependencies first, across both places");
        assert_eq!(hold(&[one.clone(), two.clone()], "cfg80211.ko"), Some(one.join("cfg80211.ko")));
        assert_eq!(hold(&[two.clone()], "cfg80211.ko"), None, "a place that does not hold it says so");
        let _ = fs::remove_dir_all(&one);
        let _ = fs::remove_dir_all(&two);
    }

    #[test]
    fn the_symbol_the_kernel_could_not_resolve_names_the_module_that_holds_it() {
        let log = "cfg80211: Unknown symbol rfkill_register (err -2)\nwlan_drv: Unknown symbol cfg80211_register (err -2)\n";
        let symbols = unknown_symbols(log);
        assert_eq!(symbols.len(), 2);
        assert!(symbols.contains("rfkill_register"));
        assert!(symbols.contains("cfg80211_register"));
        let dir = scratch("providing");
        fs::write(dir.join("rfkill.ko"), b"\x7fELF rfkill_register rfkill_unregister").unwrap();
        fs::write(dir.join("mac80211.ko"), b"\x7fELF ieee80211_register_hw").unwrap();
        let found = providing(&[dir.clone()], &symbols);
        assert_eq!(found.len(), 1, "only the module that holds what was asked for");
        assert_eq!(found[0].0, dir.join("rfkill.ko"));
        assert_eq!(found[0].1, "rfkill_register");
        let _ = fs::remove_dir_all(&dir);
    }
}
