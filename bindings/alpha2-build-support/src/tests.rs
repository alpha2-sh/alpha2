use super::*;

#[test]
fn include_directories_are_read_from_pkg_config_flags() {
    assert_eq!(
        flag_paths(b"-I/sdk/include -I/sdk/include/vulkan  -DNDEBUG\n", b"-I"),
        [
            PathBuf::from("/sdk/include"),
            PathBuf::from("/sdk/include/vulkan")
        ]
    );
}

#[test]
fn escaped_bytes_in_pkg_config_flags_are_part_of_the_path() {
    // pkgconf escapes a space, every pkg-config a shell character, and all
    // but pkgconf 3 each byte of a non-ASCII name.
    let output = b"-I/home/My\\ SDK/include -I/a(b)\\&c/include -I/\\\xeb\\\xac\\\xb8/include\n";
    assert_eq!(
        flag_paths(output, b"-I"),
        [
            PathBuf::from("/home/My SDK/include"),
            PathBuf::from("/a(b)&c/include"),
            PathBuf::from("/\u{bb38}/include"),
        ]
    );
    assert_eq!(
        flag_paths(b"-L/a(b)\\&c/lib -lavcodec\n", b"-L"),
        [PathBuf::from("/a(b)&c/lib")]
    );
}

/// The pinned SDK's `.pc` files, which name every path from
/// `${pcfiledir}`, under a directory whose name pkg-config escapes.
#[cfg(unix)]
fn sdk_under_an_escaped_path(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir()
        .join(format!(
            "alpha2-build-support-{name}-{}",
            std::process::id()
        ))
        .join("a(b)&c-\u{bb38}\u{c11c}");
    let pkgconfig = root.join("lib/pkgconfig");
    std::fs::create_dir_all(&pkgconfig).expect("directory");
    for library in ["libavcodec", "libavutil"] {
        std::fs::create_dir_all(root.join("include").join(library)).expect("directory");
        std::fs::write(root.join("include").join(library).join("version.h"), "").expect("header");
        std::fs::write(
            pkgconfig.join(format!("{library}.pc")),
            format!(
                "prefix=${{pcfiledir}}/../..\nlibdir=${{prefix}}/lib\n\
                     includedir=${{prefix}}/include\nName: {library}\nDescription: fixture\n\
                     Version: 1\nLibs: -L${{libdir}} -l{library}\nCflags: -I${{includedir}}\n"
            ),
        )
        .expect("pc file");
    }
    (root, pkgconfig)
}

#[test]
#[cfg(unix)]
fn headers_and_libdir_come_from_an_sdk_whose_path_pkg_config_escapes() {
    let (root, pkgconfig) = sdk_under_an_escaped_path("escaped");
    let cflags = pkg_config(
        Some(pkgconfig.as_os_str()),
        "--cflags-only-I",
        &["libavcodec", "libavutil"],
    )
    .expect("pkg-config");
    let libs = pkg_config(
        Some(pkgconfig.as_os_str()),
        "--libs-only-L",
        &["libavcodec"],
    )
    .expect("pkg-config");
    let includes = flag_paths(&cflags, b"-I");
    let libdirs = flag_paths(&libs, b"-L");
    let workspace = root.parent().expect("scratch").to_owned();
    let include_exists = includes.iter().all(|directory| directory.is_dir());
    let libdir_is_the_sdk = libdirs
        .first()
        .is_some_and(|libdir| libdir.canonicalize().ok() == root.join("lib").canonicalize().ok());
    std::fs::remove_dir_all(workspace).expect("cleanup");
    assert!(
        !includes.is_empty() && include_exists,
        "include directories {includes:?} from {:?}",
        String::from_utf8_lossy(&cflags)
    );
    assert!(
        libdir_is_the_sdk,
        "libdirs {libdirs:?} from {:?}",
        String::from_utf8_lossy(&libs)
    );
}

#[test]
#[cfg(unix)]
fn a_pkg_config_path_that_finds_libavcodec_elsewhere_is_refused() {
    let (root, pkgconfig) = sdk_under_an_escaped_path("fallthrough");
    let empty = root.join("lib");
    let mut both = empty.clone().into_os_string();
    both.push(":");
    both.push(&pkgconfig);
    let found = pc_file_dir(&both).expect("pkg-config finds the SDK");
    let through_both = found_outside(&both, &found);
    // The first entry alone has no libavcodec.pc: what pkg-config read
    // came from somewhere it does not name.
    let through_empty = found_outside(empty.as_os_str(), &found);
    let named = found.canonicalize().ok() == pkgconfig.canonicalize().ok();
    std::fs::remove_dir_all(root.parent().expect("scratch")).expect("cleanup");
    assert!(named, "pkg-config read {found:?}");
    assert_eq!(through_both, None);
    assert!(
        through_empty.is_some_and(|message| message.contains("outside it")),
        "a path without the SDK was accepted"
    );
}

#[test]
fn pkg_config_variables_lose_their_escapes() {
    assert_eq!(
        unescape(b"/My\\ SDK/lib/pkgconfig"),
        b"/My SDK/lib/pkgconfig"
    );
    assert_eq!(unescape(b"/a\\\\b"), b"/a\\b");
}

#[test]
#[should_panic(expected = "no FFmpeg include directory has libavcodec/version.h")]
fn an_include_directory_without_the_headers_stops_the_build() {
    require_headers(&[PathBuf::from("/nonexistent/include")], &["libavcodec"]);
}

fn environment<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
    |name| {
        pairs
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| (*value).to_owned())
    }
}

#[test]
fn the_ffmpeg_is_found_in_rusty_ffmpegs_order_for_the_host() {
    let sdk = [
        ("FFMPEG_PKG_CONFIG_PATH", "/sdk/lib/pkgconfig"),
        ("FFMPEG_INCLUDE_DIR", "/sdk/include"),
        ("FFMPEG_LIBS_DIR", "/sdk/lib"),
    ];
    let prebuilt = FfmpegSource::Directories {
        libs: "/sdk/lib".into(),
        include: Some("/sdk/include".into()),
    };
    // Off Windows the pkg-config path wins over the libraries directory.
    assert_eq!(
        ffmpeg_source(false, environment(&sdk)),
        FfmpegSource::PkgConfig(Some("/sdk/lib/pkgconfig".into()))
    );
    // On a Windows host rusty_ffmpeg never reads it.
    assert_eq!(ffmpeg_source(true, environment(&sdk)), prebuilt);
    assert_eq!(ffmpeg_source(false, environment(&sdk[1..])), prebuilt);
    assert_eq!(
        ffmpeg_source(true, environment(&sdk[..2])),
        FfmpegSource::Unset
    );
    // Without any, pkg-config on the system path; the include directory
    // alone only names rusty_ffmpeg's bindings, not what it links.
    assert_eq!(
        ffmpeg_source(false, environment(&sdk[1..2])),
        FfmpegSource::PkgConfig(None)
    );
    // A DLL path comes first, everywhere.
    let dll = [
        ("FFMPEG_DLL_PATH", "/dll/bin/avcodec-62.dll"),
        ("FFMPEG_PKG_CONFIG_PATH", "/sdk/lib/pkgconfig"),
        ("FFMPEG_INCLUDE_DIR", "/dll/include"),
    ];
    for windows_host in [false, true] {
        assert_eq!(
            ffmpeg_source(windows_host, environment(&dll)),
            FfmpegSource::Directories {
                libs: "/dll/bin".into(),
                include: Some("/dll/include".into())
            }
        );
    }
}

#[test]
fn the_rpath_follows_prebuilt_libraries() {
    assert_eq!(
        ffmpeg_libdir(FfmpegSource::Directories {
            libs: "/sdk/lib".into(),
            include: None
        }),
        Some(PathBuf::from("/sdk/lib"))
    );
    assert_eq!(ffmpeg_libdir(FfmpegSource::Unset), None);
}

#[test]
fn external_headers_are_searched_without_their_warnings() {
    let directories = [PathBuf::from("/sdk/include")];
    assert_eq!(
        external_include_flags(&directories, ExternalStyle::Isystem),
        [OsString::from("-isystem"), OsString::from("/sdk/include")]
    );
    assert_eq!(
        external_include_flags(&directories, ExternalStyle::Msvc),
        [
            OsString::from("/external:I/sdk/include"),
            OsString::from("/external:W0")
        ]
    );
    assert_eq!(
        external_include_flags(&directories, ExternalStyle::ClangCl),
        [OsString::from("/external:I/sdk/include")]
    );
    assert_eq!(
        external_include_flags(&[], ExternalStyle::Msvc),
        [] as [std::ffi::OsString; 0]
    );
}

#[test]
fn an_msvc_compile_failure_names_the_windows_sdk_floor() {
    assert!(toolchain_floor(ExternalStyle::Msvc).contains("Windows SDK 10.0.20348.0"));
    assert_eq!(toolchain_floor(ExternalStyle::Isystem), "");
    assert_eq!(toolchain_floor(ExternalStyle::ClangCl), "");
}

#[test]
fn only_macos_gets_a_development_rpath() {
    assert!(wants_dev_rpath("macos", ""));
    assert!(!wants_dev_rpath("linux", ""));
    assert!(!wants_dev_rpath("windows", ""));
}

#[test]
fn an_rpath_the_caller_chose_is_kept() {
    // Cargo separates encoded flags with 0x1f.
    let release = "-C\u{1f}link-arg=-Wl,-rpath,@executable_path/../lib";
    assert!(!wants_dev_rpath("macos", release));
    assert!(wants_dev_rpath("macos", "-C\u{1f}opt-level=3"));
}

#[test]
fn the_guarded_majors_are_the_pinned_sdk_libraries() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../release/ffmpeg-lgpl/manifest.toml"),
    )
    .expect("the pinned SDK manifest");
    for library in [
        format!("\"libavutil.so.{FFMPEG_AVUTIL_MAJOR}\""),
        format!("\"libavcodec.so.{FFMPEG_AVCODEC_MAJOR}\""),
        format!("\"avutil-{FFMPEG_AVUTIL_MAJOR}.dll\""),
        format!("\"avcodec-{FFMPEG_AVCODEC_MAJOR}.dll\""),
    ] {
        assert!(
            manifest.contains(&library),
            "the pinned SDK manifest does not ship {library}"
        );
    }
}

#[test]
fn version_headers_are_found_under_each_include_directory() {
    let root = std::env::temp_dir().join(format!(
        "alpha2-build-support-headers-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(root.join("libavutil")).expect("directory");
    std::fs::write(root.join("libavutil/version.h"), "").expect("header");
    let found = ffmpeg_version_headers(std::slice::from_ref(&root));
    std::fs::remove_dir_all(&root).expect("cleanup");
    assert_eq!(found, [root.join("libavutil/version.h")]);
}

#[test]
fn the_directive_is_a_linker_argument_for_this_package() {
    assert_eq!(
        rpath_directives(Path::new("/sdk/lib")),
        ["cargo:rustc-link-arg=-Wl,-rpath,/sdk/lib"]
    );
    // `-Wl,` would split this one at the comma.
    assert_eq!(
        rpath_directives(Path::new("/a,b/My SDK/lib")),
        [
            "cargo:rustc-link-arg=-Xlinker",
            "cargo:rustc-link-arg=-rpath",
            "cargo:rustc-link-arg=-Xlinker",
            "cargo:rustc-link-arg=/a,b/My SDK/lib",
        ]
    );
}
