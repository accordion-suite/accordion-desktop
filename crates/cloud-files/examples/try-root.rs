//! Tries the Cloud Files API by hand: `try-root register <folder>`,
//! `try-root run <folder>`, and `try-root unregister`.
//!
//! `run` makes a few placeholders whose contents are made up, and serves
//! them until it's stopped.

#[cfg(windows)]
fn main() {
    use std::sync::Arc;
    use std::time::{Duration, SystemTime};

    use cloud_files::{Fetch, FetchRequest, Placeholder, RootInfo};

    struct MadeUp;
    impl Fetch for MadeUp {
        fn fetch(&self, request: FetchRequest) {
            println!(
                "fetch {} ({}) from {} for {} of {}",
                request.path.display(),
                String::from_utf8_lossy(&request.identity),
                request.offset,
                request.length,
                request.file_size
            );
            std::thread::spawn(move || {
                let start = request.offset - request.offset % cloud_files::BLOCK;
                let line = format!("{}\n", String::from_utf8_lossy(&request.identity));
                let all: Vec<u8> = line
                    .bytes()
                    .cycle()
                    .take(request.file_size as usize)
                    .collect();
                let mut at = start;
                while at < request.file_size {
                    let end = (at + 64 * 1024).min(request.file_size);
                    std::thread::sleep(Duration::from_millis(20));
                    if let Err(err) = request.transfer.send(at, &all[at as usize..end as usize]) {
                        println!("send failed: {err}");
                        return;
                    }
                    at = end;
                }
                println!("sent {}", request.path.display());
            });
        }
        fn cancel(&self, transfer: i64) {
            println!("canceled {transfer}");
        }
    }

    let args: Vec<String> = std::env::args().collect();
    let id = cloud_files::root_id("try").unwrap();
    match args.get(1).map(String::as_str) {
        Some("register") => {
            let path = std::path::PathBuf::from(&args[2]);
            cloud_files::register(&RootInfo {
                id: id.clone(),
                path,
                display_name: "Accordion (Try)".into(),
                icon: r"%SystemRoot%\system32\imageres.dll,-1043".into(),
                version: "0.1.0".into(),
            })
            .unwrap();
            println!("registered {id}");
        }
        Some("unregister") => {
            cloud_files::unregister(&id).unwrap();
            println!("unregistered {id}");
        }
        Some("list") => {
            for (id, path) in cloud_files::registered().unwrap() {
                println!("{id} at {}", path.display());
            }
        }
        Some("state") => println!("{:?}", cloud_files::state(args[2].as_ref()).unwrap()),
        Some("run") => {
            let root = std::path::PathBuf::from(&args[2]);
            let _connection = cloud_files::connect(&root, Arc::new(MadeUp)).unwrap();
            let now = SystemTime::now();
            if !root.join("Docs").exists() {
                cloud_files::create(
                    &root,
                    &[
                        Placeholder {
                            name: "hello.txt",
                            identity: b"hello",
                            folder: false,
                            size: 100_000,
                            modified: now,
                        },
                        Placeholder {
                            name: "Docs",
                            identity: b"docs",
                            folder: true,
                            size: 0,
                            modified: now,
                        },
                    ],
                )
                .unwrap();
                cloud_files::create(
                    &root.join("Docs"),
                    &[Placeholder {
                        name: "inside.txt",
                        identity: b"inside",
                        folder: false,
                        size: 5_000,
                        modified: now,
                    }],
                )
                .unwrap();
                println!("made placeholders");
            }
            println!("{:?}", cloud_files::state(&root.join("hello.txt")).unwrap());
            std::thread::sleep(Duration::from_secs(
                args.get(3).and_then(|s| s.parse().ok()).unwrap_or(60),
            ));
        }
        Some("selftest") => {
            let root = std::path::PathBuf::from(&args[2]);
            let _connection = cloud_files::connect(&root, Arc::new(MadeUp)).unwrap();
            let now = SystemTime::now();
            let a = root.join("a.bin");
            let show = |what: &str, path: &std::path::Path| {
                println!(
                    "{what}: {:?} len={:?}",
                    cloud_files::state(path),
                    std::fs::metadata(path).map(|m| m.len())
                );
            };
            let _ = std::fs::remove_file(&a);
            cloud_files::create(
                &root,
                &[Placeholder {
                    name: "a.bin",
                    identity: b"a",
                    folder: false,
                    size: 10_000,
                    modified: now,
                }],
            )
            .unwrap();
            show("1 created", &a);
            println!("2 hydrate {:?}", cloud_files::hydrate(&a));
            show("2 hydrated", &a);
            println!("3 read {:?}", std::fs::read(&a).map(|d| d.len()));
            println!("4 dehydrate {:?}", cloud_files::dehydrate(&a));
            show("4 dehydrated", &a);
            println!(
                "5 read own placeholder {:?}",
                std::fs::read(&a).map(|d| d.len())
            );
            println!("6 pin {:?}", cloud_files::set_pinned(&a, Some(true)));
            println!("6 hydrate {:?}", cloud_files::hydrate(&a));
            show("6 pinned", &a);
            let newer = Placeholder {
                name: "a.bin",
                identity: b"a2",
                folder: false,
                size: 20_000,
                modified: now,
            };
            println!(
                "7 update pinned {:?}",
                cloud_files::update(&a, &newer, true)
            );
            println!("7 unpin {:?}", cloud_files::set_pinned(&a, None));
            println!("7 update {:?}", cloud_files::update(&a, &newer, true));
            println!("7 pin {:?}", cloud_files::set_pinned(&a, Some(true)));
            println!("7 hydrate {:?}", cloud_files::hydrate(&a));
            show("7 updated", &a);
            let b = root.join("b.txt");
            let _ = std::fs::remove_file(&b);
            std::fs::write(&b, "plain file").unwrap();
            show("8 plain", &b);
            println!("8 convert {:?}", cloud_files::convert(&b, b"b"));
            show("8 converted", &b);
            std::thread::sleep(Duration::from_millis(50));
            std::fs::write(&b, "plain file, changed").unwrap();
            show("9 changed", &b);
            println!("10 in sync {:?}", cloud_files::set_in_sync(&b, true));
            show("10 marked", &b);
            let c = root.join("c");
            let _ = std::fs::remove_dir_all(&c);
            std::fs::create_dir(&c).unwrap();
            println!("11 convert folder {:?}", cloud_files::convert(&c, b"c"));
            show("11 folder", &c);
            let a2 = root.join("a2.bin");
            let _ = std::fs::remove_file(&a2);
            println!("12 unpin {:?}", cloud_files::set_pinned(&a, None));
            println!("12 dehydrate {:?}", cloud_files::dehydrate(&a));
            std::fs::rename(&a, &a2).unwrap();
            show("12 renamed", &a2);
            println!("13 read-only {:?}", {
                let mut p = std::fs::metadata(&a2).unwrap().permissions();
                p.set_readonly(true);
                std::fs::set_permissions(&a2, p)
            });
            show("13 read-only", &a2);
            println!(
                "14 remove {:?}",
                std::fs::remove_file(&a2).or_else(|_| {
                    let mut p = std::fs::metadata(&a2).unwrap().permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    std::fs::set_permissions(&a2, p).and_then(|_| std::fs::remove_file(&a2))
                })
            );
        }
        _ => println!(
            "try-root register <folder> | run <folder> [seconds] | state <path> | list | unregister"
        ),
    }
}

#[cfg(not(windows))]
fn main() {}
