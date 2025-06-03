#!/bin/bash

# Multicast Streaming Server Build Script
# This script handles building, testing, and packaging the streaming server

set -euo pipefail

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Configuration
RUST_VERSION="1.75"
PROJECT_NAME="multicast-streaming-server"
BUILD_DIR="target"
PACKAGE_DIR="package"

# Print colored output
print_status() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if command exists
command_exists() {
    command -v "$1" >/dev/null 2>&1
}

# Install Rust if not present
install_rust() {
    if ! command_exists rustc; then
        print_status "Installing Rust..."
        curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
        source ~/.cargo/env
    fi
    
    # Check Rust version
    local current_version=$(rustc --version | cut -d' ' -f2)
    print_status "Using Rust version: $current_version"
}

# Install system dependencies
install_system_deps() {
    print_status "Installing system dependencies..."
    
    if command_exists apt-get; then
        # Debian/Ubuntu
        sudo apt-get update
        sudo apt-get install -y \
            pkg-config \
            libssl-dev \
            libpq-dev \
            cmake \
            build-essential \
            ffmpeg \
            postgresql-client \
            curl \
            wget
    elif command_exists yum; then
        # RHEL/CentOS
        sudo yum install -y \
            pkgconfig \
            openssl-devel \
            postgresql-devel \
            cmake \
            gcc \
            gcc-c++ \
            ffmpeg \
            postgresql \
            curl \
            wget
    elif command_exists brew; then
        # macOS
        brew install \
            pkg-config \
            openssl \
            postgresql \
            cmake \
            ffmpeg
    else
        print_warning "Unknown package manager. Please install dependencies manually."
    fi
}

# Install Rust tools
install_rust_tools() {
    print_status "Installing Rust tools..."
    
    # Install sqlx-cli for database migrations
    if ! command_exists sqlx; then
        cargo install sqlx-cli --no-default-features --features postgres
    fi
    
    # Install useful development tools
    cargo install --quiet cargo-watch cargo-audit cargo-outdated 2>/dev/null || true
}

# Run linting and formatting
lint() {
    print_status "Running code formatting and linting..."
    
    # Format code
    cargo fmt --all -- --check || {
        print_warning "Code is not formatted. Running cargo fmt..."
        cargo fmt --all
    }
    
    # Run clippy
    cargo clippy --all-targets --all-features -- -D warnings
    
    print_success "Linting completed successfully"
}

# Run tests
test() {
    print_status "Running tests..."
    
    # Unit tests
    cargo test --lib
    
    # Integration tests (if any)
    if [ -d "tests" ]; then
        cargo test --test '*'
    fi
    
    # Documentation tests
    cargo test --doc
    
    print_success "All tests passed"
}

# Security audit
audit() {
    print_status "Running security audit..."
    
    if command_exists cargo-audit; then
        cargo audit
        print_success "Security audit completed"
    else
        print_warning "cargo-audit not installed, skipping security audit"
    fi
}

# Check for outdated dependencies
check_outdated() {
    print_status "Checking for outdated dependencies..."
    
    if command_exists cargo-outdated; then
        cargo outdated
    else
        print_warning "cargo-outdated not installed, skipping dependency check"
    fi
}

# Build the project
build() {
    local profile=${1:-release}
    
    print_status "Building project in $profile mode..."
    
    if [ "$profile" = "release" ]; then
        cargo build --release
    else
        cargo build
    fi
    
    print_success "Build completed successfully"
}

# Create package
package() {
    local version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
    
    print_status "Creating package for version $version..."
    
    # Clean and create package directory
    rm -rf "$PACKAGE_DIR"
    mkdir -p "$PACKAGE_DIR"
    
    # Copy binary
    cp "$BUILD_DIR/release/$PROJECT_NAME" "$PACKAGE_DIR/"
    
    # Copy configuration files
    cp config.toml "$PACKAGE_DIR/config.toml.example"
    cp Rocket.toml "$PACKAGE_DIR/"
    
    # Copy migrations
    cp -r migrations "$PACKAGE_DIR/"
    
    # Copy static files
    cp -r static "$PACKAGE_DIR/"
    
    # Copy documentation
    cp README.md "$PACKAGE_DIR/"
    cp LICENSE "$PACKAGE_DIR/" 2>/dev/null || true
    
    # Copy systemd service file
    cp streaming-server.service "$PACKAGE_DIR/"
    
    # Create installation script
    cat > "$PACKAGE_DIR/install.sh" << 'EOF'
#!/bin/bash
set -euo pipefail

# Installation script for Multicast Streaming Server

INSTALL_DIR="/opt/streaming-server"
CONFIG_DIR="/etc/streaming-server"
DATA_DIR="/var/lib/streaming-server"
LOG_DIR="/var/log/streaming-server"
USER="streaming"
GROUP="streaming"

echo "Installing Multicast Streaming Server..."

# Create user and group
if ! id "$USER" &>/dev/null; then
    sudo useradd -r -s /bin/false -d "$DATA_DIR" "$USER"
fi

# Create directories
sudo mkdir -p "$INSTALL_DIR" "$CONFIG_DIR" "$DATA_DIR" "$LOG_DIR"
sudo mkdir -p "/tmp/streaming/hls"

# Copy files
sudo cp multicast-streaming-server "$INSTALL_DIR/"
sudo cp -r migrations "$INSTALL_DIR/"
sudo cp -r static "$INSTALL_DIR/"
sudo cp config.toml.example "$CONFIG_DIR/config.toml"
sudo cp Rocket.toml "$INSTALL_DIR/"

# Set permissions
sudo chown -R "$USER:$GROUP" "$INSTALL_DIR" "$DATA_DIR" "$LOG_DIR"
sudo chown -R "$USER:$GROUP" "/tmp/streaming"
sudo chmod +x "$INSTALL_DIR/multicast-streaming-server"

# Install systemd service
sudo cp streaming-server.service /etc/systemd/system/
sudo systemctl daemon-reload

echo "Installation completed!"
echo "Please edit $CONFIG_DIR/config.toml before starting the service"
echo "Start with: sudo systemctl start streaming-server"
echo "Enable auto-start: sudo systemctl enable streaming-server"
EOF
    
    chmod +x "$PACKAGE_DIR/install.sh"
    
    # Create tarball
    local tarball="$PROJECT_NAME-$version-$(uname -m).tar.gz"
    tar -czf "$tarball" -C "$PACKAGE_DIR" .
    
    print_success "Package created: $tarball"
}

# Build Docker image
docker_build() {
    local tag=${1:-latest}
    
    print_status "Building Docker image with tag: $tag"
    
    docker build -t "$PROJECT_NAME:$tag" .
    
    print_success "Docker image built successfully"
}

# Run Docker Compose stack
docker_up() {
    print_status "Starting Docker Compose stack..."
    
    if [ ! -f "docker-compose.yml" ]; then
        print_error "docker-compose.yml not found"
        exit 1
    fi
    
    docker-compose up -d
    
    print_success "Docker stack started"
    print_status "Services available at:"
    echo "  - Web UI: http://localhost:8080"
    echo "  - API: http://localhost:8080/api/v1"
    echo "  - Grafana: http://localhost:3000 (admin/admin)"
    echo "  - Prometheus: http://localhost:9090"
}

# Stop Docker Compose stack
docker_down() {
    print_status "Stopping Docker Compose stack..."
    docker-compose down
    print_success "Docker stack stopped"
}

# Clean build artifacts
clean() {
    print_status "Cleaning build artifacts..."
    
    cargo clean
    rm -rf "$PACKAGE_DIR"
    rm -f *.tar.gz
    
    print_success "Cleanup completed"
}

# Show help
show_help() {
    cat << EOF
Multicast Streaming Server Build Script

Usage: $0 [COMMAND]

Commands:
    setup           Install dependencies and tools
    lint            Run code formatting and linting
    test            Run all tests
    audit           Run security audit
    outdated        Check for outdated dependencies
    build [MODE]    Build project (debug|release, default: release)
    package         Create distribution package
    docker-build    Build Docker image
    docker-up       Start Docker Compose stack
    docker-down     Stop Docker Compose stack
    clean           Clean build artifacts
    all             Run lint, test, audit, build, and package
    ci              Run CI pipeline (lint, test, audit, build)
    help            Show this help message

Examples:
    $0 setup                    # Install all dependencies
    $0 build debug              # Build in debug mode
    $0 docker-build v1.0.0      # Build Docker image with tag v1.0.0
    $0 all                      # Full build pipeline
EOF
}

# Main function
main() {
    local command=${1:-help}
    
    case $command in
        setup)
            install_rust
            install_system_deps
            install_rust_tools
            ;;
        lint)
            lint
            ;;
        test)
            test
            ;;
        audit)
            audit
            ;;
        outdated)
            check_outdated
            ;;
        build)
            build "${2:-release}"
            ;;
        package)
            build release
            package
            ;;
        docker-build)
            docker_build "${2:-latest}"
            ;;
        docker-up)
            docker_up
            ;;
        docker-down)
            docker_down
            ;;
        clean)
            clean
            ;;
        all)
            lint
            test
            audit
            build release
            package
            ;;
        ci)
            lint
            test
            audit
            build release
            ;;
        help|--help|-h)
            show_help
            ;;
        *)
            print_error "Unknown command: $command"
            show_help
            exit 1
            ;;
    esac
}

# Run main function with all arguments
main "$@"