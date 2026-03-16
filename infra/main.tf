terraform {
  required_version = ">= 1.5"

  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.0"
    }
  }
}

provider "aws" {
  region = var.region
}

# ---------------------------------------------------------------------------
# Data sources
# ---------------------------------------------------------------------------

# Alpine Linux ARM64 AMI
data "aws_ami" "alpine" {
  most_recent = true
  owners      = ["538276064493"] # Official Alpine Linux

  filter {
    name   = "name"
    values = ["alpine-3.*-aarch64-uefi-cloudinit-r0"]
  }
  filter {
    name   = "architecture"
    values = ["arm64"]
  }
  filter {
    name   = "state"
    values = ["available"]
  }
}

# Current caller identity (for tagging)
data "aws_caller_identity" "current" {}

# Pick a single AZ for the placement group
data "aws_availability_zones" "available" {
  state = "available"
}

# Caller's public IP for SSH access
data "http" "my_ip" {
  url = "https://checkip.amazonaws.com"
}

locals {
  my_ip = "${trimspace(data.http.my_ip.response_body)}/32"
  az    = data.aws_availability_zones.available.names[0]
  ami   = data.aws_ami.alpine.id

  common_tags = {
    Project   = "harrow-bench"
    ManagedBy = "terraform"
  }
}

# ---------------------------------------------------------------------------
# Placement group — cluster strategy for minimal network jitter
# ---------------------------------------------------------------------------

resource "aws_placement_group" "bench" {
  name     = "harrow-bench"
  strategy = "cluster"

  tags = local.common_tags
}
