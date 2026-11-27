//! Keyset pages of meetings, search and actions: the same rows as the
//! unpaged reads, cursors bound to the query and the index state.

use super::*;
use crate::schema::ActionStatus;

const PAGED: &str = "paging";

/// Meetings 3 to 8, started in pairs at the same instant, each with two
/// identical "budget line" segments at the same start (bm25, start and
/// window-text ties) and three actions, the middle one done.
fn many_meetings() -> Vec<Meeting> {
    (3..=8u32)
        .map(|n| {
            let day = 1 + (n - 3) / 2;
            let mut m = Meeting::new(
                mid(&format!("m-{n:06x}")),
                Source::Recording,
                ts(&format!("2026-11-{day:02}T09:00:00Z")),
            );
            m.title = Some(format!("Planning sync {n}"));
            m.duration_s = 60.0;
            m.tags = vec![PAGED.into()];
            if n % 2 == 0 {
                m.tags.push("even".into());
            }
            m.speakers = vec![speaker(1, Some(ANA), Assignment::Confirmed, 10.0, None)];
            m.segments = vec![
                seg(0, Some(1), 0.0, "budget line"),
                seg(1, Some(1), 0.0, "budget line"),
            ];
            let mut m = with_enrichment(m, "fake", None, None, false, "2026-11-15T10:00:00Z");
            m.enrichment.as_mut().unwrap().actions = (0..3)
                .map(|i| {
                    serde_json::from_value(serde_json::json!({
                        "id": format!("a{n}{i}"),
                        "text": format!("Follow up budget item {i}"),
                        "status": if i == 1 { "done" } else { "open" },
                    }))
                    .unwrap()
                })
                .collect();
            m
        })
        .collect()
}

/// [`setup`]'s library plus [`many_meetings`], rebuilt.
fn paging_library() -> (TempDir, Library) {
    let (tmp, lib) = setup();
    lib.write(|tx| {
        for m in many_meetings() {
            tx.create_meeting_dir(&m)?;
            tx.save_meeting(&m)?;
        }
        Ok(())
    })
    .unwrap();
    rebuild(&lib);
    (tmp, lib)
}

/// Caches a vector for every window text: `[1, 0]` for the "budget line"
/// windows (one shared text, so equal scores), `[1, 1]` for the others.
fn cache_vectors(lib: &Library) {
    let mut entries = BTreeMap::new();
    for m in meetings().into_iter().chain(many_meetings()) {
        for w in meeting_windows(&m) {
            let v = if w.text.starts_with("budget line") {
                vec![1.0, 0.0]
            } else {
                vec![1.0, 1.0]
            };
            entries.insert(w.text_sha256, v);
        }
    }
    let entries: Vec<_> = entries.into_iter().collect();
    let mut cache = VectorCache::open(lib.root()).unwrap();
    lib.write(|tx| cache.insert(tx, "m", &entries)).unwrap();
}

fn vector() -> Option<QueryVector> {
    Some(QueryVector {
        model: "m".into(),
        vector: vec![1.0, 0.0],
    })
}

fn budget(mode: SearchMode) -> SearchQuery {
    SearchQuery {
        text: Some("budget".into()),
        mode,
        query_vector: (mode != SearchMode::Keyword).then(vector).flatten(),
        ..SearchQuery::default()
    }
}

/// Every page of a paged read, in order. Only the last page may be short,
/// and it is empty only when the whole result is.
fn all_pages<T>(size: usize, mut read: impl FnMut(&CursorRequest) -> Result<Page<T>>) -> Vec<T> {
    let mut request = CursorRequest::first(size);
    let mut out = Vec::new();
    loop {
        let page = read(&request).unwrap();
        assert!(page.items.len() <= size);
        let Some(cursor) = page.next_cursor else {
            assert!(!page.items.is_empty() || out.is_empty(), "empty last page");
            out.extend(page.items);
            return out;
        };
        assert_eq!(page.items.len(), size, "only the last page may be short");
        out.extend(page.items);
        request = CursorRequest::after(cursor, size);
    }
}

fn assert_stale<T: std::fmt::Debug>(result: Result<T>) {
    assert_eq!(result.unwrap_err().code(), CURSOR_STALE);
}

#[test]
fn meeting_pages_equal_the_unpaged_order_without_gaps_or_repeats() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    let queries = [
        SearchQuery::default(),
        text("planning"),
        text("sync"),
        SearchQuery {
            tag: Some("Paging".into()),
            ..SearchQuery::default()
        },
        SearchQuery {
            text: Some("sync".into()),
            tag: Some("even".into()),
            ..SearchQuery::default()
        },
        person("ana example"),
    ];
    for q in &queries {
        let all = r.search_meetings(q).unwrap();
        assert!(!all.is_empty(), "{q:?}");
        for size in [1, 2, 3, MAX_PAGE_SIZE] {
            assert_eq!(
                all_pages(size, |p| r.search_meetings_page(q, p)),
                all,
                "{q:?} by {size}"
            );
        }
    }
    let listed = all_pages(2, |p| r.search_meetings_page(&SearchQuery::default(), p));
    assert_eq!(listed.len(), 8);
}

#[test]
fn search_pages_equal_keyword_listing_and_fused_rankings() {
    let (_tmp, lib) = paging_library();
    cache_vectors(&lib);
    let r = reader(&lib);
    assert!(r.vectors_attached());
    let queries = [
        SearchQuery::default(),
        budget(SearchMode::Keyword),
        SearchQuery {
            tag: Some(PAGED.into()),
            ..budget(SearchMode::Keyword)
        },
        person("ana example"),
        budget(SearchMode::Semantic),
        budget(SearchMode::Hybrid),
        SearchQuery {
            query_vector: None,
            ..budget(SearchMode::Hybrid)
        },
        SearchQuery {
            since: Some(ts("2026-11-02T00:00:00Z")),
            ..budget(SearchMode::Hybrid)
        },
    ];
    for q in &queries {
        let all = r.search(q).unwrap();
        assert!(all.len() > 1, "{q:?}");
        for size in [1, 2, 5, MAX_PAGE_SIZE] {
            assert_eq!(
                all_pages(size, |p| r.search_page(q, p)),
                all,
                "{q:?} by {size}"
            );
        }
    }
}

#[test]
fn action_pages_equal_the_unpaged_order_and_filters() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    let queries = [
        ActionQuery::default(),
        ActionQuery {
            status: Some(ActionStatus::Open),
            ..ActionQuery::default()
        },
        ActionQuery {
            text: Some("budget item".into()),
            since: Some(ts("2026-11-02T00:00:00Z")),
            ..ActionQuery::default()
        },
        ActionQuery {
            meeting: Some(mid("m-000004")),
            ..ActionQuery::default()
        },
    ];
    for q in &queries {
        let all = r.list_actions(q).unwrap();
        assert!(!all.is_empty(), "{q:?}");
        for size in [1, 2, 4, MAX_PAGE_SIZE] {
            assert_eq!(
                all_pages(size, |p| r.list_actions_page(q, p)),
                all,
                "{q:?} by {size}"
            );
        }
    }
    assert_eq!(r.list_actions(&ActionQuery::default()).unwrap().len(), 18);
}

#[test]
fn empty_and_exactly_full_results_end_without_a_cursor() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    for q in [
        text("nonexistent"),
        text("!!"),
        SearchQuery {
            tag: Some("missing".into()),
            ..SearchQuery::default()
        },
    ] {
        let page = r.search_page(&q, &CursorRequest::first(3)).unwrap();
        assert!(page.items.is_empty() && page.next_cursor.is_none(), "{q:?}");
        let page = r
            .search_meetings_page(&q, &CursorRequest::first(3))
            .unwrap();
        assert!(page.items.is_empty() && page.next_cursor.is_none(), "{q:?}");
    }
    let none = ActionQuery {
        text: Some("!!".into()),
        ..ActionQuery::default()
    };
    let page = r
        .list_actions_page(&none, &CursorRequest::first(3))
        .unwrap();
    assert!(page.items.is_empty() && page.next_cursor.is_none());

    // 18 actions in pages of 9: the second page is full and the last.
    let q = ActionQuery::default();
    let first = r.list_actions_page(&q, &CursorRequest::first(9)).unwrap();
    assert_eq!(first.items.len(), 9);
    let second = r
        .list_actions_page(&q, &CursorRequest::after(first.next_cursor.unwrap(), 9))
        .unwrap();
    assert_eq!(second.items.len(), 9);
    assert!(second.next_cursor.is_none());
    assert_eq!(second.revision, first.revision);
}

/// A first page's anchor (when its relative dates were resolved) rides in
/// every cursor after it, so the caller resolves them at that instant again;
/// the pages continue the unpaged order. A continuation at another anchor,
/// or none, is another query.
#[test]
fn cursors_carry_their_first_pages_anchor() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    let anchor = ts("2026-11-18T09:00:00.5-03:00").to_utc();
    let q = SearchQuery::default();
    let unpaged: Vec<_> = r
        .search_meetings(&q)
        .unwrap()
        .into_iter()
        .map(|h| h.meeting_id)
        .collect();
    let first = r
        .search_meetings_page(&q, &CursorRequest::first_at(2, anchor))
        .unwrap();
    let cursor = first.next_cursor.unwrap();
    assert_eq!(cursor.anchor(), Some(anchor));
    let next = CursorRequest::after(cursor, 2);
    assert_eq!(next.anchor, Some(anchor));
    let second = r.search_meetings_page(&q, &next).unwrap();
    assert_eq!(second.next_cursor.as_ref().unwrap().anchor(), Some(anchor));
    let read: Vec<_> = first
        .items
        .into_iter()
        .chain(second.items)
        .map(|h| h.meeting_id)
        .collect();
    assert_eq!(read, unpaged[..4]);
    for other in [None, Some(anchor + chrono::Duration::seconds(2))] {
        let moved = CursorRequest {
            anchor: other,
            ..next.clone()
        };
        assert_eq!(
            r.search_meetings_page(&q, &moved).unwrap_err().code(),
            CURSOR_MISMATCH,
            "{other:?}"
        );
    }
    let actions = r
        .list_actions_page(&ActionQuery::default(), &CursorRequest::first_at(4, anchor))
        .unwrap();
    assert_eq!(actions.next_cursor.unwrap().anchor(), Some(anchor));
    let search = r
        .search_page(&text("budget"), &CursorRequest::first_at(1, anchor))
        .unwrap();
    assert_eq!(search.next_cursor.unwrap().anchor(), Some(anchor));
    assert!(cursors(&lib).iter().all(|c| c.anchor().is_none()));
}

/// Second-page cursors of a meeting listing, a keyword search and the
/// action list.
fn cursors(lib: &Library) -> Vec<PageCursor> {
    let r = reader(lib);
    let first = CursorRequest::first(1);
    vec![
        r.search_meetings_page(&SearchQuery::default(), &first)
            .unwrap()
            .next_cursor
            .unwrap(),
        r.search_page(&text("budget"), &first)
            .unwrap()
            .next_cursor
            .unwrap(),
        r.list_actions_page(&ActionQuery::default(), &first)
            .unwrap()
            .next_cursor
            .unwrap(),
    ]
}

/// The second pages of [`cursors`], read now.
fn next_pages(lib: &Library, cursors: &[PageCursor]) -> Vec<Result<()>> {
    let r = reader(lib);
    let after = |i: usize| CursorRequest::after(cursors[i].clone(), 1);
    vec![
        r.search_meetings_page(&SearchQuery::default(), &after(0))
            .map(drop),
        r.search_page(&text("budget"), &after(1)).map(drop),
        r.list_actions_page(&ActionQuery::default(), &after(2))
            .map(drop),
    ]
}

#[test]
fn every_committed_index_write_makes_cursors_stale() {
    let (_tmp, lib) = paging_library();
    let before = reader(&lib).read_revision().unwrap();
    assert_eq!(before.instance.len(), 32);
    let issued = cursors(&lib);

    // Opening a writer, reading what is pending, caching vectors and a file
    // save the index has not seen yet (a crash before its upsert) change no
    // index row: the cursors stay valid, on any connection.
    drop(Index::open(lib.root()).unwrap());
    assert!(Index::health(lib.root()).unwrap().is_current());
    cache_vectors(&lib);
    let mut unseen = many_meetings().remove(2);
    unseen.tags.push("unseen".into());
    lib.write(|tx| tx.save_meeting(&unseen)).unwrap();
    assert!(pending(&lib).meetings.contains(&unseen.id));
    for result in next_pages(&lib, &issued) {
        result.unwrap();
    }
    assert_eq!(reader(&lib).read_revision().unwrap(), before);

    type Write = Box<dyn Fn(&Library)>;
    let writes: Vec<(&str, bool, Write)> = vec![
        (
            "pending repair",
            false,
            Box::new(|lib: &Library| {
                let report = lib.write(|tx| Index::repair_pending(tx)).unwrap();
                assert_eq!(report.meetings, 1);
                assert!(pending(lib).is_current());
            }),
        ),
        (
            "metadata-only upsert",
            false,
            Box::new(|lib: &Library| {
                let mut m = many_meetings().remove(0);
                m.tags.push("renamed".into());
                lib.write(|tx| {
                    tx.save_meeting(&m)?;
                    Index::open(tx.root())?.upsert_meeting(tx, &m)
                })
                .unwrap();
            }),
        ),
        (
            "person upsert",
            false,
            Box::new(|lib: &Library| {
                let mut ana = people().remove(0);
                ana.aliases.push("Ana Paging".into());
                lib.write(|tx| {
                    tx.save_person(&ana)?;
                    Index::open(tx.root())?.upsert_person(tx, &ana)
                })
                .unwrap();
            }),
        ),
        (
            "row removal",
            false,
            Box::new(|lib: &Library| {
                assert!(
                    lib.write(|tx| Index::open(tx.root())?.remove_meeting(tx, &mid("m-000005")))
                        .unwrap()
                );
            }),
        ),
        (
            "rebuild",
            true,
            Box::new(|lib: &Library| {
                rebuild(lib);
            }),
        ),
        (
            "deleted and recreated index",
            true,
            Box::new(|lib: &Library| {
                fs::remove_dir_all(lib.root().join(paths::INDEX_DIR)).unwrap();
                rebuild(lib);
            }),
        ),
    ];
    let mut issued = issued;
    let mut before = before;
    for (what, renewed, write) in writes {
        write(&lib);
        let after = reader(&lib).read_revision().unwrap();
        assert_ne!(after, before, "{what}");
        assert_eq!(after.instance != before.instance, renewed, "{what}");
        for result in next_pages(&lib, &issued) {
            assert_eq!(result.unwrap_err().code(), CURSOR_STALE, "{what}");
        }
        issued = cursors(&lib);
        before = after;
    }
}

#[test]
fn semantic_and_hybrid_cursors_are_bound_to_the_vector_cache() {
    let (_tmp, lib) = paging_library();
    cache_vectors(&lib);
    let first = CursorRequest::first(1);
    let r = reader(&lib);
    let fused: Vec<(SearchQuery, PageCursor)> = [SearchMode::Semantic, SearchMode::Hybrid]
        .into_iter()
        .map(|mode| {
            let q = budget(mode);
            let cursor = r.search_page(&q, &first).unwrap().next_cursor.unwrap();
            (q, cursor)
        })
        .collect();
    let keyword = r
        .search_page(&budget(SearchMode::Keyword), &first)
        .unwrap()
        .next_cursor
        .unwrap();
    drop(r);

    let mut cache = VectorCache::open(lib.root()).unwrap();
    lib.write(|tx| cache.insert(tx, "other", &[("x".into(), vec![1.0])]))
        .unwrap();
    let r = reader(&lib);
    for (q, cursor) in fused {
        assert_stale(r.search_page(&q, &CursorRequest::after(cursor, 1)));
    }
    // Keyword pages never read vectors.
    r.search_page(
        &budget(SearchMode::Keyword),
        &CursorRequest::after(keyword, 1),
    )
    .unwrap();
    drop(r);

    // A deleted and recreated cache is another cache.
    let q = budget(SearchMode::Semantic);
    let cursor = reader(&lib)
        .search_page(&q, &first)
        .unwrap()
        .next_cursor
        .unwrap();
    drop(cache);
    fs::remove_file(VectorCache::path(lib.root())).unwrap();
    cache_vectors(&lib);
    assert_stale(reader(&lib).search_page(&q, &CursorRequest::after(cursor, 1)));
}

#[test]
fn pages_read_in_one_snapshot_continue_while_a_writer_commits() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    let q = text("budget");
    let expected = r.search(&q).unwrap();
    let mut writer = Index::open(lib.root()).unwrap();
    let mut m = many_meetings().remove(1);
    m.segments
        .push(seg(2, Some(1), 9.0, "another budget remark"));
    let (paged, issued) = r
        .read_snapshot(|| {
            let first = r.search_page(&q, &CursorRequest::first(4))?;
            let issued = first.next_cursor.clone().unwrap();
            lib.write(|tx| {
                tx.save_meeting(&m)?;
                writer.upsert_meeting(tx, &m)
            })?;
            // The other pages, in the same snapshot: still the rows of
            // before the write.
            let mut items = first.items;
            let mut cursor = first.next_cursor;
            while let Some(c) = cursor {
                let page = r.search_page(&q, &CursorRequest::after(c, 4))?;
                items.extend(page.items);
                cursor = page.next_cursor;
            }
            Ok((items, issued))
        })
        .unwrap();
    assert_eq!(paged, expected);

    // Once the snapshot ends the write is visible: the cursor is stale and
    // a new first page leads to every row, the new one included.
    assert_stale(r.search_page(&q, &CursorRequest::after(issued, 4)));
    let now = r.search(&q).unwrap();
    assert_eq!(now.len(), expected.len() + 1);
    assert_eq!(all_pages(4, |p| r.search_page(&q, p)), now);
}

#[test]
fn cursors_of_other_queries_or_reads_and_bad_requests_are_refused() {
    let (_tmp, lib) = paging_library();
    let r = reader(&lib);
    let cursor = r
        .search_page(&text("budget"), &CursorRequest::first(1))
        .unwrap()
        .next_cursor
        .unwrap();
    let after = || CursorRequest::after(cursor.clone(), 1);
    for other in [
        text("line"),
        SearchQuery {
            tag: Some(PAGED.into()),
            ..text("budget")
        },
        SearchQuery {
            mode: SearchMode::Hybrid,
            ..text("budget")
        },
        SearchQuery::default(),
    ] {
        assert_eq!(
            r.search_page(&other, &after()).unwrap_err().code(),
            CURSOR_MISMATCH,
            "{other:?}"
        );
    }
    // The same search spelled with other spacing is the same query.
    r.search_page(&text("  budget "), &after()).unwrap();
    assert_eq!(
        r.search_meetings_page(&text("budget"), &after())
            .unwrap_err()
            .code(),
        CURSOR_MISMATCH
    );
    assert_eq!(
        r.list_actions_page(&ActionQuery::default(), &after())
            .unwrap_err()
            .code(),
        CURSOR_MISMATCH
    );

    // Another library's index never continues this one's pages.
    let (_other_tmp, other) = paging_library();
    assert_stale(reader(&other).search_page(&text("budget"), &after()));

    let long = "A".repeat(MAX_CURSOR_LEN + 1);
    for bad in ["", "%%%", "eyJ2IjoxfQ", long.as_str()] {
        assert_eq!(PageCursor::parse(bad).unwrap_err().code(), INVALID_CURSOR);
    }
    let parsed = PageCursor::parse(cursor.as_str()).unwrap();
    assert_eq!(parsed, cursor);

    for size in [0, MAX_PAGE_SIZE + 1] {
        assert_eq!(
            r.search_page(&text("budget"), &CursorRequest::first(size))
                .unwrap_err()
                .code(),
            "invalid_input"
        );
    }
    let limited = SearchQuery {
        limit: Some(5),
        ..text("budget")
    };
    assert_eq!(
        r.search_page(&limited, &CursorRequest::first(5))
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    assert_eq!(
        r.search_meetings_page(&limited, &CursorRequest::first(5))
            .unwrap_err()
            .code(),
        "invalid_input"
    );
    let limited = ActionQuery {
        limit: Some(1),
        ..ActionQuery::default()
    };
    assert_eq!(
        r.list_actions_page(&limited, &CursorRequest::first(1))
            .unwrap_err()
            .code(),
        "invalid_input"
    );
}

#[test]
fn identities_are_given_to_older_indexes_and_renewed_by_upgrades() {
    let (_tmp, lib) = paging_library();
    // An index written before identities existed.
    Connection::open(Index::path(lib.root()))
        .unwrap()
        .execute(
            "DELETE FROM meta WHERE key IN ('instance', 'cursor_secret', 'revision')",
            [],
        )
        .unwrap();
    let q = SearchQuery::default();
    let page = reader(&lib)
        .search_page(&q, &CursorRequest::first(2))
        .unwrap();
    assert_eq!(
        page.revision,
        IndexRevision {
            instance: String::new(),
            revision: 0
        }
    );
    let next = CursorRequest::after(page.next_cursor.unwrap(), 2);
    reader(&lib).search_page(&q, &next).unwrap();

    // The first writer gives it one; opening again changes nothing.
    drop(Index::open(lib.root()).unwrap());
    let given = reader(&lib).read_revision().unwrap();
    assert_eq!(given.instance.len(), 32);
    assert_stale(reader(&lib).search_page(&q, &next));
    drop(Index::open(lib.root()).unwrap());
    assert_eq!(reader(&lib).read_revision().unwrap(), given);

    // An in-place upgrade renews it and still asks for a rebuild.
    let next = CursorRequest::after(
        reader(&lib)
            .search_page(&q, &CursorRequest::first(2))
            .unwrap()
            .next_cursor
            .unwrap(),
        2,
    );
    downgrade_to_v6(&lib);
    let upgraded = reader(&lib);
    assert_ne!(upgraded.read_revision().unwrap().instance, given.instance);
    assert_stale(upgraded.search_page(&q, &next));
    drop(upgraded);
    assert!(pending(&lib).rebuild);
}
