.PHONY: all linux windows install-linux install-windows clean

ifeq ($(OS),Windows_NT)
all: windows
else
all: linux windows_from_linux
endif

linux:
	cd linux && cargo build --release
	@echo "\nLinux build complete: linux/target/release/bluevein-linux"

windows:
	cd windows && cargo build --release
	@echo "\nWindows build complete: windows/target/release/bluevein-windows.exe"

windows_from_linux:
	cd windows && cargo build --release --target x86_64-pc-windows-gnu
	@echo "\nWindows build complete: windows/target/x86_64-pc-windows-gnu/release/bluevein-windows.exe"

install-linux: linux
	sudo cp ./target/release/bluevein-linux /usr/local/bin/bluevein-linux
	sudo cp linux/bluevein-linux.service /etc/systemd/system/
	sudo systemctl daemon-reload
	sudo systemctl enable bluevein-linux
	sudo systemctl start bluevein-linux
	@echo "Linux service installed and started"

install-windows: windows
	@echo "Run as Administrator in PowerShell:"
	@echo "  .\windows\target\x86_64-pc-windows-gnu\release\bluevein-windows.exe install"
	@echo "  Start-Service bluevein-windows"

clean:
	cd linux && cargo clean
	cd windows && cargo clean
	cd shared && cargo clean