{ lib
, rustPlatform
, pkg-config
, dbus
, doCheck ? false
}:

rustPlatform.buildRustPackage {
  pname = "bluevein";
  version = (lib.importTOML ../Cargo.toml).package.version;

  src = lib.cleanSource ../.;
  cargoLock.lockFile = ../Cargo.lock;
  inherit doCheck;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ dbus ];

  meta = with lib; {
    description = "Bluetooth device synchronization service for dual-boot systems";
    homepage = "https://github.com/meowrch/BlueVein";
    license = licenses.mit;
    mainProgram = "bluevein";
    platforms = platforms.linux;
  };
}
