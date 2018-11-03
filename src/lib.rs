extern crate bit_vec;
extern crate ego_tree;
extern crate notify;
#[macro_use]
extern crate log;
#[macro_use]
extern crate derive_builder;

use bit_vec::BitVec;
use ego_tree::iter::Descendants;
use ego_tree::{NodeMut, NodeRef, Tree};
use notify::{watcher, DebouncedEvent, RecursiveMode, Watcher};
use std::fs;
use std::io;
use std::iter::{FromIterator, IntoIterator, Iterator, Skip};
use std::path::{Path, PathBuf};
use std::sync::mpsc::channel;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::thread;
use std::time::Duration;

#[derive(Clone, Copy, Builder)]
#[builder(default)]
pub struct Options {
    include_files: bool,
    watch_changes: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            include_files: true,
            watch_changes: false,
        }
    }
}

#[derive(Clone)]
pub struct DirCache {
    inner: Arc<DirCacheInner>,
}

impl DirCache {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        DirCache::new_with_options(root, Default::default())
    }

    pub fn new_with_options<P: AsRef<Path>>(root: P, options: Options) -> Self {
        let dc = DirCache {
            inner: Arc::new(DirCacheInner::new_with_options(&root, options)),
        };

        if options.watch_changes {
            let dc = dc.clone();
            let root: PathBuf = root.as_ref().into();
            let _watcher = thread::spawn(move || match dc.load() {
                Ok(_) => {
                    let (tx, rx) = channel();
                    let mut watcher = watcher(tx, Duration::from_secs(10)).unwrap();
                    watcher.watch(root, RecursiveMode::Recursive).unwrap();

                    loop {
                        match rx.recv() {
                            Ok(event) => {
                                debug!("directory change - event {:?}", event);
                                match event {
                                    DebouncedEvent::Create(_)
                                    | DebouncedEvent::Remove(_)
                                    | DebouncedEvent::Rename(_, _)
                                    | DebouncedEvent::Rescan => match dc.load() {
                                        Ok(_) => debug!("Directory cache updated"),
                                        Err(e) => {
                                            error!("Failed to update directory cache: error {}", e)
                                        }
                                    },
                                    _ => (),
                                }
                            }
                            Err(e) => error!("watch error: {:?}", e),
                        }
                    }
                }
                Err(e) => error!("cannot start watching directory due to error: {}", e),
            });
        }
        dc
    }

    pub fn is_ready(&self) -> bool {
        self.inner.cache.read().unwrap().is_some()
    }

    pub fn load(&self) -> Result<(), io::Error> {
        self.inner.load()
    }

    pub fn search<S: AsRef<str>>(&self, query: S) -> Result<Vec<PathBuf>, io::Error> {
        self.inner.search(query)
    }

    pub fn wait_ready(&self) {
        self.inner.wait_ready()
    }
}
struct DirCacheInner {
    cache: RwLock<Option<DirTree>>,
    root: PathBuf,
    options: Options,
    ready_flag: Mutex<bool>,
    ready_cond: Condvar,
}

impl DirCacheInner {
    fn new_with_options<P: AsRef<Path>>(root: P, options: Options) -> Self {
        DirCacheInner {
            root: root.as_ref().into(),
            cache: RwLock::new(None),
            options: options,
            ready_flag: Mutex::new(false),
            ready_cond: Condvar::new(),
        }
    }

    fn wait_ready(&self) {
        let mut flag = self.ready_flag.lock().unwrap();
        while !*flag {
            flag = self.ready_cond.wait(flag).unwrap();
        }
    }

    fn load(&self) -> Result<(), io::Error> {
        let tree = DirTree::new_with_options(&self.root, self.options)?;
        {
            let mut cache = self.cache.write().unwrap();
            *cache = Some(tree)
        }
        {
            let mut flag = self.ready_flag.lock().unwrap();
            *flag = true;
            self.ready_cond.notify_all();
        }
        Ok(())
    }

    fn search<S: AsRef<str>>(&self, query: S) -> Result<Vec<PathBuf>, io::Error> {
        let cache = self.cache.read().unwrap();
        if cache.is_none() {
            return Err(io::Error::new(io::ErrorKind::Other, "cache not ready"));
        }
        Ok(cache
            .as_ref()
            .unwrap()
            .search(query)
            .map(|e| e.path())
            .collect())
    }
}

pub struct DirTree {
    tree: Tree<DirEntry>,
}

#[derive(Debug)]
pub struct DirEntry {
    pub name: String,
    pub search_tag: String,
}

impl DirEntry {
    pub fn new<S: ToString>(name: S) -> Self {
        let name: String = name.to_string();
        DirEntry {
            search_tag: name.to_lowercase(),
            name,
        }
    }
}

impl<T: ToString> From<T> for DirEntry {
    fn from(s: T) -> Self {
        DirEntry::new(s)
    }
}

pub type DirRef<'a> = NodeRef<'a, DirEntry>;

pub struct SearchItem<'a>(DirRef<'a>);

impl<'a> SearchItem<'a> {
    pub fn path(&self) -> PathBuf {
        let segments: Vec<_> = self
            .0
            .ancestors()
            .filter_map(|n| {
                if n.parent().is_some() {
                    Some(&n.value().name)
                } else {
                    None
                }
            })
            .collect();
        let mut p = PathBuf::from_iter(segments.into_iter().rev());
        p.push(&self.0.value().name);
        p
    }
}

pub struct SearchResult<'a> {
    current_node: DirRef<'a>,
    search_terms: Vec<String>,
    truncate_this_branch: bool,
    matched_terms: BitVec,
    matched_terms_stack: Vec<BitVec>,
}

impl<'a> Iterator for SearchResult<'a> {
    type Item = SearchItem<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            trace!("Before step {:?} searching {:?} already found {:?}",
            self.current_node.value(), self.search_terms, self.matched_terms
            );
            if let Some(next) = if self.truncate_this_branch {
                None
            } else {
                self.current_node.first_child()
            } {
                self.current_node = next;
                self.matched_terms_stack.push(self.matched_terms.clone());
                trace!("Going down - push {:?}", self.matched_terms);
            } else if let Some(next) = self.current_node.next_sibling() {
                self.current_node = next;
                self.matched_terms = self.matched_terms_stack.last().unwrap().clone();
                trace!("Going right");
            } else if let Some(mut parent) = self.current_node.parent() {
                self.matched_terms = self.matched_terms_stack.pop().unwrap();
                self.matched_terms = self.matched_terms_stack.last().unwrap().clone();
                trace!("Going up and right pop {:?}", self.matched_terms );
                while let None = parent.next_sibling() {
                    parent = match parent.parent() {
                        Some(p) => {
                            self.matched_terms = self.matched_terms_stack.pop().unwrap();
                            self.matched_terms = self.matched_terms_stack.last().unwrap().clone();
                            trace!("Going up and right pop {:?}", self.matched_terms );
                            p
                        }
                        None => return None,
                    };
                }
                // is safe to unwrap, as previous loop will either find parent with next sibling or return
                self.current_node = parent.next_sibling().unwrap();
            } else {
                unreachable!("Never should run after root")
            }

            trace!("After step {:?} searching {:?} already found {:?}",
            self.current_node.value(), self.search_terms, self.matched_terms
            );
            self.truncate_this_branch = false;
            if self.is_match() {
                // we already got match - we did not need to dive deaper
                self.truncate_this_branch = true;
                return Some(SearchItem(self.current_node));
            }
        }
    }
}

impl<'a> SearchResult<'a> {
    fn is_match(&mut self) -> bool {
        let mut matched = vec![];
        let mut res = true;
        self.search_terms.iter().enumerate().for_each(|(i, term)| {
            if !self.matched_terms[i] {
                let contains = self.current_node.value().search_tag.contains(term);
                if contains {
                    matched.push(i)
                }
                res &= contains
            }
        });
        matched
            .into_iter()
            .for_each(|i| self.matched_terms.set(i, true));
        res
    }
}

impl<'a> IntoIterator for &'a DirTree {
    type Item = DirRef<'a>;
    type IntoIter = Skip<Descendants<'a, DirEntry>>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl DirTree {
    pub fn new<P: AsRef<Path>>(root_dir: P) -> Result<Self, io::Error> {
        DirTree::new_with_options(root_dir, Default::default())
    }

    pub fn new_with_options<P: AsRef<Path>>(root_dir: P, opts: Options) -> Result<Self, io::Error> {
        let p: &Path = root_dir.as_ref();
        let root_name = p.to_str().ok_or(io::Error::new(
            io::ErrorKind::Other,
            "root directory is not utf8",
        ))?;
        if !p.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "root path does not exists or is not director",
            ));
        }
        let mut cached = Tree::new(DirEntry::new(root_name));
        {
            let mut root = cached.root_mut();

            fn add_entries<P: AsRef<Path>>(
                node: &mut NodeMut<DirEntry>,
                path: P,
                opts: &Options,
            ) -> Result<(), io::Error> {
                for e in fs::read_dir(path)? {
                    let e = e?;
                    if let Ok(file_type) = e.file_type() {
                        if file_type.is_dir() {
                            let mut dir_node = node.append(e.file_name().to_string_lossy().into());
                            let p = e.path();
                            add_entries(&mut dir_node, &p, opts)?
                        } else if opts.include_files && file_type.is_file() {
                            node.append(e.file_name().to_string_lossy().into());
                        }
                    }
                }
                Ok(())
            }

            add_entries(&mut root, p, &opts)?;
        }

        Ok(DirTree { tree: cached })
    }

    pub fn iter(&self) -> Skip<Descendants<DirEntry>> {
        self.tree.root().descendants().skip(1)
    }

    pub fn search<S: AsRef<str>>(&self, query: S) -> SearchResult {
        let search_terms = query
            .as_ref()
            .split(' ')
            .map(|s| s.trim().to_lowercase())
            .collect::<Vec<_>>();
        let m = BitVec::from_elem(search_terms.len(), false);
        SearchResult {
            matched_terms: m.clone(),
            matched_terms_stack: vec![m],
            current_node: self.tree.root(),
            search_terms,
            truncate_this_branch: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_creation() {
        let c = DirTree::new("test_data").unwrap();
        let root = c.iter();
        let num_children = root.count();
        assert!(num_children > 3);
        //c.iter().for_each(|n| println!("{}", n.value()))
    }

    #[test]
    fn test_search() {
        fn count_matches(mut s: SearchResult) -> usize {
            let mut num = 0;
            while let Some(n) = s.next() {
                println!("{:?}", n.path());
                num += 1;
            }
            num
        }
        let c = DirTree::new("test_data").unwrap();
        let s = c.search("usak");

        assert_eq!(0, count_matches(s));

        let s = c.search("target build");
        assert_eq!(2, count_matches(s));

        

        let s = c.search("cargo");
        assert_eq!(4, count_matches(s));
        let options = OptionsBuilder::default()
            .include_files(false)
            .build()
            .unwrap();
        let c = DirTree::new_with_options("test_data", options).unwrap();
        let s = c.search("cargo");
        assert_eq!(0, count_matches(s));
    }

    #[test]
    fn test_search2() {
        let c = DirTree::new("test_data").unwrap();
        let s = c.search("build target");
        assert_eq!(2, s.count());
    }

    #[test]
    fn test_search3() {
        let c = DirTree::new("test_data").unwrap();
        let s = c.search("doyle modry");
        assert_eq!(1, s.count());
        let s = c.search("chesterton modry");
        assert_eq!(1, s.count());
    }


    #[test]
    fn test_cache() {
        let c = DirCache::new("test_data");
        assert!(!c.is_ready());
        c.load().unwrap();
        assert!(c.is_ready());
        let res = c.search("cargo").unwrap();
        assert_eq!(4, res.len())
    }
    #[test]
    fn multithread() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::thread;
        let counter = Arc::new(AtomicUsize::new(0));
        const NUM_THREADS: usize = 100;
        let c = DirCache::new("test_data");
        let c2 = c.clone();
        assert!(!c.is_ready());
        let loader_thread = thread::spawn(move || {
            c.load().unwrap();
        });
        let mut threads = vec![];
        for _i in 0..NUM_THREADS {
            let c = c2.clone();
            let counter = counter.clone();
            let t = thread::spawn(move || {
                c.wait_ready();
                let res = c.search("cargo").unwrap();
                assert_eq!(4, res.len());
                counter.fetch_add(1, Ordering::Relaxed);
            });
            threads.push(t);
        }
        loader_thread.join().unwrap();
        for t in threads {
            t.join().unwrap()
        }

        assert_eq!(NUM_THREADS, counter.load(Ordering::Relaxed));
    }

}
