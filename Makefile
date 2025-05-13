# Makefile for both Dev Container and CLI workflows

IMAGE_NAME ?= reconcile-rs-dev-container
WORKDIR_MOUNT ?= $$(pwd):/workspace

.PHONY: build run init dc-up dc-rebuild

# Build the Docker image
build:
	docker build -f .devcontainer/Dockerfile.dev -t $(IMAGE_NAME) .

# Run container with workspace mounted (CLI)
run: build
	docker run --rm -it \
		-v $(WORKDIR_MOUNT) \
		-w /workspace \
		$(IMAGE_NAME)

# Manual init for CLI users (chmod + run)
init:
	chmod +x .devcontainer/init.sh
	./.devcontainer/init.sh

# Start Dev Container via CLI (passes Git author info into the container)
dc-up:
	GIT_AUTHOR_NAME="$$(git config --global user.name)" \
	GIT_AUTHOR_EMAIL="$$(git config --global user.email)" \
	devcontainer up

# Rebuild Dev Container via CLI (passes Git author info)
dc-rebuild:
	GIT_AUTHOR_NAME="$$(git config --global user.name)" \
	GIT_AUTHOR_EMAIL="$$(git config --global user.email)" \
	devcontainer rebuild