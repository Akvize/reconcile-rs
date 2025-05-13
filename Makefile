# Makefile for both Dev Container and CLI workflows

IMAGE_NAME ?= reconcile-rs-dev-container
# By default mount the current directory into /workspace
WORKDIR_MOUNT ?= $(shell pwd):/workspace

.PHONY: build dev dc-up dc-rebuild

# Build the Docker image
build:
	docker build -f .devcontainer/Dockerfile.dev -t $(IMAGE_NAME) .

# Manual init for CLI users
dev: build
	docker run --rm -it \
	  --entrypoint bash \
	  -e GIT_AUTHOR_NAME="$(shell git config --global user.name)" \
	  -e GIT_AUTHOR_EMAIL="$(shell git config --global user.email)" \
	  -v $(WORKDIR_MOUNT) \
	  -w /workspace \
	  $(IMAGE_NAME) \
	  -lc "\
	  	chmod +x .devcontainer/init.sh && \
	    .devcontainer/init.sh create && \
	    .devcontainer/init.sh start && \
	    exec bash \
	  "

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