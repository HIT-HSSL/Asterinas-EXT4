{ gbenchmark, gtest, leveldb, }:
leveldb.overrideAttrs (old: {
  pname = "leveldb-db-bench";
  outputs = [ "out" ];
  cmakeFlags = [
    "-DBUILD_SHARED_LIBS=OFF"
    "-DLEVELDB_BUILD_TESTS=ON"
    "-DLEVELDB_BUILD_BENCHMARKS=ON"
    "-DLEVELDB_INSTALL=OFF"
  ];
  buildFlags = [ "db_bench" ];
  # LEVELDB_BUILD_TESTS provides db_bench's gtest/gmock dependencies. The
  # dedicated benchmark package intentionally does not build the test targets.
  doCheck = false;
  postPatch = (old.postPatch or "") + ''
    rm -rf third_party/benchmark third_party/googletest
    ln -s ${gbenchmark.src} third_party/benchmark
    ln -s ${gtest.src} third_party/googletest
  '';
  installPhase = ''
    runHook preInstall

    mkdir -p $out/bin
    cp db_bench $out/bin/

    runHook postInstall
  '';
  postInstall = "";
})
