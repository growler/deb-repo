use {
    crate::{
        control::ParseError,
        idmap::{id_type, HashRef, IdMap, IntoId, ToIndex, UpdateResult},
        packages::{Package, Packages},
        repo::{VerifyingDebReader, VerifyingReader},
        version::{self, Constraint, Dependency, ProvidedName, Satisfies, Version},
    },
    async_std::io::{self, Write},
    iterator_ext::IteratorExt,
    resolvo::{
        Candidates, Dependencies, DependencyProvider, Interner, KnownDependencies, NameId,
        Requirement, SolvableId, SolverCache, StringId, UnsolvableOrCancelled, VersionSetId,
        VersionSetUnionId,
    },
    smallvec::{smallvec, SmallVec},
    std::{
        borrow::Borrow,
        hash::{Hash, Hasher},
        pin::pin,
    },
};

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum ArchId {
    #[default]
    Any,
    Arch(std::num::NonZeroU8),
}
impl IntoId<ArchId> for usize {
    fn into_id(self) -> ArchId {
        match self {
            0 => ArchId::Any,
            n => ArchId::Arch(std::num::NonZeroU8::new(n.try_into().unwrap()).unwrap()),
        }
    }
}
impl ToIndex for ArchId {
    fn to_index(&self) -> usize {
        match self {
            Self::Any => 0,
            Self::Arch(id) => id.get() as usize,
        }
    }
}
impl Satisfies<ArchId> for ArchId {
    fn satisfies(&self, target: &ArchId) -> bool {
        match (self, target) {
            (ArchId::Any, _) => true,
            (_, ArchId::Any) => true,
            (ArchId::Arch(this), ArchId::Arch(that)) => this == that,
        }
    }
}

id_type!(VersionSetId);
id_type!(VersionSetUnionId);
id_type!(StringId);
id_type!(NameId);
id_type!(SolvableId);

#[derive(Debug)]
struct Name<'a> {
    name: &'a str,
    packages: SmallVec<[SolvableId; 1]>,
    required: Vec<SolvableId>,
}
impl<'a> Hash for Name<'a> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state)
    }
}
impl<'a> Borrow<str> for HashRef<Name<'a>> {
    fn borrow(&self) -> &str {
        &self.name
    }
}
impl<'a> Eq for Name<'a> {}
impl<'a> PartialEq for Name<'a> {
    fn eq(&self, other: &Self) -> bool {
        self.name.eq(other.name)
    }
}

#[derive(Debug, Hash, PartialEq, Eq)]
struct VersionSet<'a> {
    arch: ArchId,
    name: NameId,
    selfref: Option<SolvableId>,
    range: version::VersionSet<Version<&'a str>>,
}

impl<'a> VersionSet<'a> {}

struct Solvable<'a> {
    arch: ArchId,
    name: NameId,
    pkgs: u32,
    package: &'a Package<'a>,
}

impl<'a> std::fmt::Debug for Solvable<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "Solvable{{ {} {}:{}={} }}",
            self.name.to_index(),
            self.package.name(),
            self.package.architecture(),
            self.package.version()
        )
    }
}

impl<'a> Solvable<'a> {
    fn full_name(&self) -> ProvidedName<NameId, Version<&'a str>> {
        ProvidedName::Exact(self.name, self.package.version())
    }
}

#[derive(Default, Debug)]
struct UniverseIndex<'a> {
    arch: ArchId,
    solvables: Vec<Solvable<'a>>,
    names: IdMap<NameId, Name<'a>>,
    archlist: IdMap<ArchId, &'a str>,
    version_sets: IdMap<VersionSetId, VersionSet<'a>>,
    version_set_unions: IdMap<VersionSetUnionId, SmallVec<[VersionSetId; 2]>>,
    required: Vec<Requirement>,
}

#[ouroboros::self_referencing]
struct InnerUniverse<S: AsRef<str> + 'static> {
    packages: Vec<Packages<S>>,
    interned: IdMap<StringId, Box<str>>,
    #[borrows(packages, interned)]
    #[not_covariant]
    index: UniverseIndex<'this>,
}

impl<'a> UniverseIndex<'a> {
    fn get_arch_id(&self, arch: &'a str) -> ArchId {
        if arch.eq_ignore_ascii_case("all") {
            ArchId::Any
        } else {
            self.archlist.get_or_insert(arch).into()
        }
    }
    fn insert_or_update_name(
        &self,
        name: &'a str,
        solvable: Option<(SolvableId, bool)>,
    ) -> UpdateResult<NameId> {
        unsafe {
            let k = self.names.insert_or_update(
                name,
                || match solvable {
                    Some((id, required)) => Name {
                        name,
                        packages: smallvec![id],
                        required: if required { vec![id] } else { vec![] },
                    },
                    None => Name {
                        name,
                        packages: smallvec![],
                        required: vec![],
                    },
                },
                |name| match solvable {
                    Some((id, required)) => {
                        name.packages.push(id);
                        if required {
                            name.required.push(id);
                        }
                    }
                    None => {}
                },
            );
            k
        }
    }
    fn intern_version_set<A, N, V>(
        &self,
        dep: Constraint<Option<A>, N, Version<V>>,
        strings: &'a IdMap<StringId, Box<str>>,
    ) -> VersionSetId
    where
        A: AsRef<str>,
        N: AsRef<str>,
        V: AsRef<str>,
    {
        self.get_single_dependency_id(dep.translate(
            |a| {
                a.as_ref()
                    .map_or(self.arch, |a| self.get_arch_id(strings.intern(a).as_ref()))
            },
            |n| strings.intern(n).as_ref(),
            |v| v.translate(|v| strings.intern(v).as_ref()),
        ))
    }
    fn get_single_dependency_id(
        &self,
        dep: Constraint<ArchId, &'a str, Version<&'a str>>,
    ) -> VersionSetId {
        self.version_sets.get_or_insert(VersionSet {
            name: self.insert_or_update_name(dep.name(), None).into(),
            arch: *dep.arch(),
            selfref: None,
            range: dep.into_range(),
        })
    }
    fn get_union_dependency_id(
        &self,
        deps: impl Iterator<Item = Constraint<ArchId, &'a str, Version<&'a str>>>,
    ) -> VersionSetUnionId {
        self.version_set_unions
            .get_or_insert(deps.map(|dep| self.get_single_dependency_id(dep)).collect())
            .into()
    }
    fn add_package(
        &mut self,
        pkgs: u32,
        required: &mut Vec<NameId>,
        package: &'a Package<'a>,
    ) -> Result<(), ParseError> {
        let solvable_id: SolvableId = self.solvables.len().into_id();
        let is_required = package.essential() || package.required();
        let arch = self.get_arch_id(package.architecture());
        let name =
            match self.insert_or_update_name(package.name(), Some((solvable_id, is_required))) {
                UpdateResult::Updated(id) => id,
                UpdateResult::Inserted(id) => {
                    if is_required {
                        required.push(id)
                    };
                    id
                }
            };
        self.solvables.push(Solvable {
            pkgs,
            arch,
            name,
            package,
        });
        for pv in package.provides() {
            self.insert_or_update_name(pv?.name(), Some((solvable_id, false)));
        }
        Ok(())
    }
    fn add_single_package_dependency(
        &self,
        id: SolvableId,
        dep: Constraint<Option<&'a str>, &'a str, Version<&'a str>>,
    ) -> VersionSetId {
        let pkg = &self.solvables[id.to_index()];
        let self_ref = pkg.package.provides_name(dep.name());
        let name = self.insert_or_update_name(dep.name(), None).unwrap();
        let arch = dep.arch().map_or(pkg.arch, |arch| {
            if arch.eq_ignore_ascii_case("any") {
                ArchId::Any
            } else {
                pkg.arch
            }
        });
        self.version_sets.get_or_insert(VersionSet {
            arch,
            name,
            selfref: if self_ref { Some(id) } else { None },
            range: dep.into_range(),
        })
    }
    fn add_package_dependencies(
        &self,
        solvable: SolvableId,
        strings: &'a IdMap<StringId, Box<str>>,
    ) -> Dependencies {
        let pkg = &self.solvables[solvable.to_index()];
        let requirements = match pkg
            .package
            .pre_depends()
            .chain(pkg.package.depends())
            .and_then(|dep| match dep {
                Dependency::Single(dep) => Ok(Requirement::Single(
                    self.add_single_package_dependency(solvable, dep),
                )),
                Dependency::Union(deps) => Ok(Requirement::Union(
                    self.version_set_unions.get_or_insert(
                        deps.into_iter()
                            .map(|dep| self.add_single_package_dependency(solvable, dep))
                            .collect(),
                    ),
                )),
            })
            .collect::<Result<Vec<_>, ParseError>>()
        {
            Ok(reqs) => reqs,
            Err(err) => {
                return Dependencies::Unknown(
                    strings
                        .intern(format!(
                            "error parsing dependencies for {}: {}",
                            pkg.package.full_name(),
                            err
                        ))
                        .as_id(),
                )
            }
        };
        let constrains = match pkg
            .package
            .conflicts()
            .chain(pkg.package.breaks())
            .and_then(|dep| Ok(self.add_single_package_dependency(solvable, dep)))
            .collect::<Result<Vec<_>, ParseError>>()
        {
            Ok(reqs) => reqs,
            Err(err) => {
                return Dependencies::Unknown(
                    strings
                        .intern(format!(
                            "error parsing constrains for {}: {}",
                            pkg.package.full_name(),
                            err
                        ))
                        .as_id(),
                )
            }
        };
        Dependencies::Known(KnownDependencies {
            requirements,
            constrains,
        })
    }
}

pub struct Universe<S: AsRef<str> + 'static> {
    inner: resolvo::Solver<InnerUniverse<S>>,
}

impl<S: AsRef<str> + 'static> Universe<S> {
    pub fn new(
        arch: impl AsRef<str>,
        from: impl IntoIterator<Item = Packages<S>>,
    ) -> Result<Self, ParseError> {
        Ok(Self {
            inner: resolvo::Solver::new(
                InnerUniverseTryBuilder {
                    packages: from.into_iter().collect(),
                    interned: IdMap::from([arch.as_ref()]),
                    index_builder: |list: &'_ Vec<Packages<S>>,
                                    interned: &'_ IdMap<StringId, Box<str>>|
                     -> Result<UniverseIndex<'_>, ParseError> {
                        let mut index = UniverseIndex::default();
                        index.archlist.get_or_insert("any"); // == ArchId::Any
                        index.arch = index.archlist.get_or_insert(&interned[StringId(0)]);
                        let mut required = Vec::<NameId>::new();
                        for (num, pkgs) in list.iter().enumerate() {
                            for package in pkgs.packages() {
                                index.add_package(num as u32, &mut required, package)?;
                            }
                        }
                        for name in required {
                            let pkgs: SmallVec<[VersionSetId; 2]> = index.names[name]
                                .required
                                .iter()
                                .map(|sid| {
                                    let solvable = &index.solvables[sid.to_index()];
                                    index.version_sets.get_or_insert(VersionSet {
                                        name,
                                        arch: solvable.arch,
                                        selfref: None,
                                        range: index.solvables[sid.to_index()]
                                            .full_name()
                                            .version()
                                            .into(),
                                    })
                                })
                                .collect();
                            index.required.push(match pkgs.len() {
                                1 => Requirement::Single(pkgs[0]),
                                _ => {
                                    Requirement::Union(index.version_set_unions.get_or_insert(pkgs))
                                }
                            })
                        }
                        Ok(index)
                    },
                }
                .try_build()?,
            ),
        })
    }
    pub fn problem<A, N, V, Id, Ic>(
        &self,
        requirements: Id,
        constraints: Ic,
    ) -> resolvo::Problem<std::iter::Empty<SolvableId>>
    where
        A: AsRef<str>,
        N: AsRef<str>,
        V: AsRef<str>,
        Id: IntoIterator<Item = Dependency<Option<A>, N, Version<V>>>,
        Ic: IntoIterator<Item = Constraint<Option<A>, N, Version<V>>>,
    {
        resolvo::Problem::new()
            .requirements(
                requirements
                    .into_iter()
                    .map(|d| match d {
                        Dependency::Single(vs) => {
                            Requirement::Single(self.inner.provider().intern_single_dependency(vs))
                        }
                        Dependency::Union(vsu) => {
                            Requirement::Union(self.inner.provider().intern_union_dependency(vsu))
                        }
                    })
                    .chain(
                        self.inner
                            .provider()
                            .with_index(|i| i.required.iter())
                            .map(|v: &Requirement| v.clone()),
                    )
                    .collect(),
            )
            .constraints(
                constraints
                    .into_iter()
                    .map(|dep| self.inner.provider().intern_single_dependency(dep))
                    .collect(),
            )
    }
    pub fn solve(
        &mut self,
        problem: resolvo::Problem<std::iter::Empty<SolvableId>>,
    ) -> Result<Vec<SolvableId>, UnsolvableOrCancelled> {
        self.inner.solve(problem)
    }
    pub fn dependency_graph(
        &self,
        solution: &mut [SolvableId],
    ) -> petgraph::graphmap::DiGraphMap<SolvableId, ()> {
        self.inner.provider().dependency_graph(solution)
    }
    pub fn sort_solution(&self, solution: &mut [SolvableId]) -> impl Iterator<Item = SolvableId> {
        self.inner.provider().sort_solution(solution)
    }
    pub fn package(&self, solvable: SolvableId) -> &Package<'_> {
        self.inner
            .provider()
            .with_index(|i| i.solvables[solvable.to_index()].package)
    }
    pub fn display_conflict(
        &self,
        conflict: resolvo::conflict::Conflict,
    ) -> impl std::fmt::Display + '_ {
        conflict.display_user_friendly(&self.inner)
    }
    pub fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_solvable(solvable)
    }
    pub fn packages(&self) -> impl Iterator<Item = &'_ Package<'_>> {
        self.inner
            .provider()
            .with_index(|i| i.solvables.iter().map(|s| s.package))
    }
    pub async fn deb_reader<'a>(&'a self, id: SolvableId) -> io::Result<VerifyingDebReader<'a>> {
        let (repo, path, size, hash) = self.inner.provider().with(|u| {
            let s = &u.index.solvables[id.to_index()];
            let (path, size, hash) = s.package.repo_file()?;
            Ok::<_, io::Error>((&u.packages[s.pkgs as usize].repo, path, size, hash))
        })?;
        repo.verifying_deb_reader(path, size, hash).await
    }
    pub async fn deb_file_reader(&self, id: SolvableId) -> io::Result<VerifyingReader> {
        let (repo, path, size, hash) = self.inner.provider().with(|u| {
            let s = &u.index.solvables[id.to_index()];
            let (path, size, hash) = s.package.repo_file()?;
            Ok::<_, io::Error>((&u.packages[s.pkgs as usize].repo, path, size, hash))
        })?;
        repo.verifying_reader(path, size, hash).await
    }
    pub async fn copy_deb_file<W: Write + Send>(&self, w: W, id: SolvableId) -> io::Result<u64> {
        let (repo, path, size, hash) = self.inner.provider().with(|u| {
            let s = &u.index.solvables[id.to_index()];
            let (path, size, hash) = s.package.repo_file()?;
            Ok::<_, io::Error>((&u.packages[s.pkgs as usize].repo, path, size, hash))
        })?;
        io::copy(repo.verifying_reader(path, size, hash).await?, pin!(w)).await
    }
}

impl<S: AsRef<str> + 'static> std::fmt::Debug for Universe<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.inner.provider().with_index(|i| write!(f, "{:?}", i))
    }
}

impl<S: AsRef<str> + 'static> InnerUniverse<S> {
    fn intern_single_dependency<A, N, V>(
        &self,
        dep: Constraint<Option<A>, N, Version<V>>,
    ) -> VersionSetId
    where
        A: AsRef<str>,
        N: AsRef<str>,
        V: AsRef<str>,
    {
        self.with(|u| u.index.intern_version_set(dep, &u.interned))
    }
    fn intern_union_dependency<A, N, V, U>(&self, vsu: U) -> VersionSetUnionId
    where
        A: AsRef<str>,
        N: AsRef<str>,
        V: AsRef<str>,
        U: IntoIterator<Item = Constraint<Option<A>, N, Version<V>>>,
    {
        self.with(|u| {
            u.index.get_union_dependency_id(vsu.into_iter().map(|dep| {
                dep.translate(
                    |a| {
                        a.as_ref().map_or(u.index.arch, |a| {
                            u.index.get_arch_id(u.interned.intern(a).as_ref())
                        })
                    },
                    |n| u.interned.intern(n).as_ref().as_ref(),
                    |v| v.translate(|v| u.interned.intern(v).as_ref()),
                )
            }))
        })
    }
    fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.with_index(|i| {
            let candidates = &i.names[name].packages;
            match candidates.len() {
                0 => None,
                _ => Some(Candidates {
                    hint_dependencies_available: candidates.to_vec(),
                    candidates: candidates.to_vec(),
                    ..Candidates::default()
                }),
            }
        })
    }
    fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        self.with(|u| u.index.add_package_dependencies(solvable, &u.interned))
    }
    fn dependency_graph(
        &self,
        solution: &mut [SolvableId],
    ) -> petgraph::graphmap::DiGraphMap<SolvableId, ()> {
        solution.sort();
        petgraph::graphmap::DiGraphMap::<SolvableId, ()>::from_edges(solution.iter().flat_map(
            |package| {
                match self.get_dependencies(*package) {
                    Dependencies::Known(deps) => deps,
                    _ => unreachable!(
                        "by this point the solvables in the solution have only known dependencies"
                    ),
                }
                .requirements
                .into_iter()
                .flat_map(|dep| match dep {
                    Requirement::Single(dep) => itertools::Either::Left(std::iter::once(dep)),
                    Requirement::Union(deps) => {
                        itertools::Either::Right(self.version_sets_in_union(deps))
                    }
                })
                .flat_map(|vs| {
                    self.get_candidates(self.version_set_name(vs))
                        .into_iter()
                        .flat_map(|vs| vs.candidates.into_iter())
                        .filter(|sid| {
                            *sid != *package
                                && solution.binary_search_by(|probe| probe.cmp(sid)).is_ok()
                        })
                })
                .map(|dependency| (*package, dependency))
            },
        ))
    }
    fn sort_solution(&self, solution: &mut [SolvableId]) -> impl Iterator<Item = SolvableId> {
        petgraph::algo::kosaraju_scc(&self.dependency_graph(solution))
            .into_iter()
            .flat_map(|g| g.into_iter())
    }
}

impl<S: AsRef<str> + 'static> Interner for Universe<S> {
    fn display_name(&self, name: NameId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_name(name)
    }
    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.inner.provider().solvable_name(solvable)
    }
    fn display_string(&self, string_id: StringId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_string(string_id)
    }
    fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_solvable(solvable)
    }
    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.inner.provider().version_set_name(version_set)
    }
    fn display_version_set(&self, version_set: VersionSetId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_version_set(version_set)
    }
    fn display_solvable_name(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_solvable_name(solvable)
    }
    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.inner
            .provider()
            .version_sets_in_union(version_set_union)
    }
    fn display_merged_solvables(&self, solvables: &[SolvableId]) -> impl std::fmt::Display + '_ {
        self.inner.provider().display_merged_solvables(solvables)
    }
}

impl<S: AsRef<str> + 'static> Interner for InnerUniverse<S> {
    fn display_name(&self, name: NameId) -> impl std::fmt::Display + '_ {
        self.with_index(|i| i.names[name].name)
    }
    fn solvable_name(&self, solvable: SolvableId) -> NameId {
        self.with_index(|i| i.solvables[solvable.to_index()].name)
    }
    fn display_string(&self, string_id: StringId) -> impl std::fmt::Display + '_ {
        self.with_interned(|s| &s[string_id])
    }
    fn display_solvable(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        self.with_index(|i| i.solvables[solvable.to_index()].package)
    }
    fn version_set_name(&self, version_set: VersionSetId) -> NameId {
        self.with_index(|i| i.version_sets[version_set].name)
    }
    fn display_version_set(&self, version_set: VersionSetId) -> impl std::fmt::Display + '_ {
        self.with_index(|i| {
            let vs = &i.version_sets[version_set];
            Constraint::new(
                Some(&i.archlist[vs.arch]),
                &i.names[vs.name].name,
                vs.range.clone(),
            )
        })
    }
    fn display_solvable_name(&self, solvable: SolvableId) -> impl std::fmt::Display + '_ {
        self.with_index(|i| i.solvables[solvable.to_index()].package.name())
    }
    fn version_sets_in_union(
        &self,
        version_set_union: VersionSetUnionId,
    ) -> impl Iterator<Item = VersionSetId> {
        self.with_index(|i| i.version_set_unions[version_set_union].iter().map(|v| *v))
    }
    fn display_merged_solvables(&self, solvables: &[SolvableId]) -> impl std::fmt::Display + '_ {
        use std::fmt::Write;
        self.with_index(|i| {
            let mut buf = String::new();
            let mut first = true;
            for pv in solvables.iter().map(|&s| i.solvables[s.to_index()].package) {
                if first {
                    first = false
                } else {
                    let _ = buf.write_str(", ");
                }
                let _ = write!(&mut buf, "{}={}", pv.name(), pv.version());
            }
            buf
        })
    }
}

impl<S: AsRef<str> + 'static> DependencyProvider for InnerUniverse<S> {
    async fn filter_candidates(
        &self,
        candidates: &[SolvableId],
        version_set: VersionSetId,
        inverse: bool,
    ) -> Vec<SolvableId> {
        let c = self.with(|u| {
            let vs = &u.index.version_sets[version_set];
            tracing::trace!(
                "filter candidates {:?} with {}{}{}",
                candidates
                    .iter()
                    .map(|c| {
                        let c = &u.index.solvables[c.to_index()];
                        format!("{}", c.package.full_name())
                    })
                    .collect::<Vec<_>>(),
                u.index.version_sets[version_set].selfref.map_or_else(
                    || "".to_string(),
                    |c| {
                        let c = &u.index.solvables[c.to_index()];
                        format!("({}={}) ", c.package.name(), c.package.version())
                    }
                ),
                Constraint::new(
                    Some(&u.index.archlist[vs.arch]),
                    &u.index.names[vs.name].name,
                    vs.range.clone(),
                ),
                if inverse { " inverse" } else { "" },
            );
            candidates
                .iter()
                .filter(|&&sid| {
                    let solvable = &u.index.solvables[sid.to_index()];
                    tracing::trace!("  validating {}", solvable.package.full_name(),);
                    if Some(sid) == vs.selfref {
                        false // always exclude self-referencing dependencies
                    } else if !solvable.arch.satisfies(&vs.arch) {
                        false // always exclude dependencies with not suitable arch
                    } else {
                        let sname = u.index.names[vs.name].name;
                        ((solvable.name == vs.name
                            && (solvable.package.version().satisfies(&vs.range)))
                            || solvable
                                .package
                                .provides()
                                .filter_map(|pv| pv.ok()) // TODO:: report parsing error
                                .find(|pv| *pv.name() == sname && (pv.satisfies(&vs.range)))
                                .is_some())
                            ^ inverse
                    }
                })
                .map(|s| *s)
                .collect()
        });
        tracing::trace!("result is {:?}", &c);
        c
    }

    async fn get_candidates(&self, name: NameId) -> Option<Candidates> {
        self.get_candidates(name)
    }

    async fn get_dependencies(&self, solvable: SolvableId) -> Dependencies {
        let deps = self.get_dependencies(solvable);
        tracing::trace!(
            "dependencies for {} {}: {}",
            solvable.to_index(),
            self.display_solvable(solvable),
            match &deps {
                Dependencies::Known(deps) => {
                    format!(
                        "Requirements({}) Constrains({})",
                        deps.requirements
                            .iter()
                            .map(|r| match r {
                                Requirement::Single(c) =>
                                    format!("{}", self.display_version_set(*c)),
                                Requirement::Union(u) => self
                                    .version_sets_in_union(*u)
                                    .map(|v| format!("{}", self.display_version_set(v)))
                                    .collect::<Vec<_>>()
                                    .join(" | "),
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        deps.constrains
                            .iter()
                            .map(|c| format!("{}", self.display_version_set(*c)))
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                }
                Dependencies::Unknown(s) => {
                    self.display_string(*s).to_string()
                }
            }
        );
        deps
    }

    async fn sort_candidates(&self, _solver: &SolverCache<Self>, solvables: &mut [SolvableId]) {
        self.with_index(|i| {
            solvables.sort_by(|this, that| {
                let this = &i.solvables[this.to_index()];
                let that = &i.solvables[that.to_index()];
                match (this.arch.satisfies(&i.arch), that.arch.satisfies(&i.arch)) {
                    (false, true) => std::cmp::Ordering::Less,
                    (true, false) => std::cmp::Ordering::Greater,
                    _ => match this.package.name().cmp(that.package.name()) {
                        std::cmp::Ordering::Equal => {
                            this.package.version().cmp(&that.package.version())
                        }
                        cmp => cmp,
                    },
                }
            })
        })
    }

    fn should_cancel_with_value(&self) -> Option<Box<dyn std::any::Any>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packages::Packages;

    use std::sync::Once;

    static INIT: Once = Once::new();

    fn init_trace() {
        INIT.call_once(|| {
            tracing_subscriber::fmt::init();
        });
    }

    macro_rules! test_solution {
        ($n:ident $problem:expr => $solution:expr , $src:expr) => {
            #[test]
            fn $n() {
                init_trace();
                let mut uni = Universe::new(
                    "amd64",
                    vec![Packages::new_test($src).expect("failed to parse test source")]
                        .into_iter(),
                )
                .unwrap();
                let problem = uni.problem(
                    $problem
                        .into_iter()
                        .map(|dep| Dependency::try_from(dep).expect("failed to parse dependency")),
                    vec![],
                );
                let solution = match uni.solve(problem) {
                    Ok(solution) => solution,
                    Err(resolvo::UnsolvableOrCancelled::Unsolvable(conflict)) => {
                        panic!("{}", uni.display_conflict(conflict))
                    }
                    Err(err) => {
                        panic!("{:?}", err)
                    }
                };
                let mut solution: Vec<_> = solution
                    .into_iter()
                    .map(|i| format!("{}", uni.display_solvable(i)))
                    .collect();
                solution.sort();
                assert_eq!(solution, $solution);
            }
        };
    }

    test_solution!(self_dependent
    [ "alpha" ] => [ "alpha:amd64=1.0" ],
"Package: alpha
Architecture: amd64
Version: 1.0
Provides: beta
Breaks: beta
");

    test_solution!(absent
    [ "alpha" ] => [ "alpha:amd64=1.0" ],
"Package: alpha
Architecture: amd64
Version: 1.0
Conflicts: beta
");

    test_solution!(absent_2
    [ "alpha" ] => [ "alpha:amd64=1.0", "beta:amd64=1.0" ],
"Package: alpha
Architecture: amd64
Version: 1.0
Depends: beta (= 1.0) | omega

Package: beta
Architecture: amd64
Version: 1.0
");

    test_solution!(mutual
    [ "alpha" ] => [ "alpha:amd64=2.6.1" ],
"Package: alpha
Architecture: amd64
Version: 2.6.1
Provides: beta (= 2.6.1)
Breaks: beta (<= 1.5~alpha4~)

Package: beta
Architecture: amd64
Version: 2.6.1
Depends: alpha (>= 1.5~alpha4~)
");

    test_solution!(dep_break
    [ "alpha" ] => [ "alpha:amd64=2.38.1-5+deb12u2", "beta:amd64=2.38.1-5+deb12u2" ],
"Package: alpha
Architecture: amd64
Version: 2.38.1-5+deb12u2
Depends: beta

Package: beta
Architecture: amd64
Version: 2.38.1-5+deb12u2
Breaks: alpha (<= 2.38~)
");

    test_solution!(dep_range
    [ "keyboard-configuration" ] => [ "keyboard-configuration:all=1.221", "xkb-data:all=2.35.1-1" ],
"Package: keyboard-configuration
Version: 1.221
Architecture: all
Depends: xkb-data (>= 2.35.1~), xkb-data (<< 2.35.1A)

Package: xkb-data
Version: 2.35.1-1
Architecture: all
");
}
