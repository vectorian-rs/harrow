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
# Remote state from base-infra
# ---------------------------------------------------------------------------

data "terraform_remote_state" "base" {
  backend = "local"

  config = {
    path = "../base-infra/terraform.tfstate"
  }
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

locals {
  base = data.terraform_remote_state.base.outputs
  ami  = data.aws_ami.alpine.id

  common_tags = {
    Project   = "harrow-bench"
    ManagedBy = "terraform"
  }
}

# ---------------------------------------------------------------------------
# Spot instance requests — server + client
# ---------------------------------------------------------------------------

resource "aws_spot_instance_request" "server" {
  ami                    = local.ami
  instance_type          = var.instance_type
  key_name               = var.key_name
  vpc_security_group_ids = [local.base.security_group_id]
  availability_zone      = local.base.availability_zone
  placement_group        = local.base.placement_group_name
  iam_instance_profile   = local.base.iam_instance_profile_name

  spot_type            = "one-time"
  wait_for_fulfillment = true

  root_block_device {
    volume_size = 30
    volume_type = "gp3"
  }

  tags = merge(local.common_tags, { Name = "harrow-bench-server" })
}

resource "aws_spot_instance_request" "client" {
  ami                    = local.ami
  instance_type          = var.instance_type
  key_name               = var.key_name
  vpc_security_group_ids = [local.base.security_group_id]
  availability_zone      = local.base.availability_zone
  placement_group        = local.base.placement_group_name
  iam_instance_profile   = local.base.iam_instance_profile_name

  spot_type            = "one-time"
  wait_for_fulfillment = true

  root_block_device {
    volume_size = 30
    volume_type = "gp3"
  }

  tags = merge(local.common_tags, { Name = "harrow-bench-client" })
}
