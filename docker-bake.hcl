variable "REGISTRY" {
  default = "ghcr.io/dicklesworthstone"
}

variable "IMAGE_NAME" {
  default = "mcp_agent_mail_rust"
}

variable "BUILDER_BASE" {
  default = "${REGISTRY}/${IMAGE_NAME}-builder-base:main"
}

variable "RUNTIME_BASE" {
  default = "${REGISTRY}/${IMAGE_NAME}-runtime-base:main"
}

variable "AM_REF" {
  default = "main"
}

variable "SIBLING_REF" {
  default = "main"
}

variable "SIBLING_FALLBACK_REF" {
  default = "main"
}

variable "FRANKENSEARCH_COMMIT" {
  default = "3bbfd8c664062f8304e7a790c51794671f9214dc"
}

variable "AM_VERSION" {
  default = ""
}

variable "AM_REVISION" {
  default = ""
}

target "builder-base" {
  context    = "."
  dockerfile = "Dockerfile.base"
  target     = "builder-base"
  platforms  = ["linux/amd64", "linux/arm64"]
  tags       = ["${REGISTRY}/${IMAGE_NAME}-builder-base:main"]
}

target "runtime-base" {
  context    = "."
  dockerfile = "Dockerfile.base"
  target     = "runtime-base"
  platforms  = ["linux/amd64", "linux/arm64"]
  tags       = ["${REGISTRY}/${IMAGE_NAME}-runtime-base:main"]
}

group "base" {
  targets = ["builder-base", "runtime-base"]
}

target "source" {
  context    = "."
  dockerfile = "Dockerfile"
  args = {
    BUILDER_BASE          = "${BUILDER_BASE}"
    RUNTIME_BASE          = "${RUNTIME_BASE}"
    AM_REF                = "${AM_REF}"
    SIBLING_REF           = "${SIBLING_REF}"
    SIBLING_FALLBACK_REF  = "${SIBLING_FALLBACK_REF}"
    FRANKENSEARCH_COMMIT  = "${FRANKENSEARCH_COMMIT}"
  }
}

target "release" {
  context    = "."
  dockerfile = "Dockerfile.release"
  args = {
    RUNTIME_BASE = "${RUNTIME_BASE}"
    AM_VERSION   = "${AM_VERSION}"
    AM_REVISION  = "${AM_REVISION}"
  }
}
