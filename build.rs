fn main() {
    println!("cargo:rerun-if-env-changed=ROCKSDB_LIB_DIR");
    // librocksdb-sys links the C++ runtime when compiling bundled sources, but
    // not when reusing an already-built static RocksDB archive on Linux.
    if std::env::var_os("ROCKSDB_LIB_DIR").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
    {
        println!("cargo:rustc-link-lib=stdc++");
    }
}
