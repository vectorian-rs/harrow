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

# ---------------------------------------------------------------------------
# ECR repositories (via datadeft module)
# ---------------------------------------------------------------------------

module "ecr_serde_bench_server" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/serde-bench-server"
  stage           = "bench"
  force-delete    = true
}

module "ecr_axum_serde_server" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/axum-serde-server"
  stage           = "bench"
  force-delete    = true
}

# ---------------------------------------------------------------------------
# IAM — ECR pull for bench instances
# ---------------------------------------------------------------------------

data "aws_region" "current" {}

resource "aws_iam_role" "bench" {
  name = "harrow-bench-instance"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Action    = "sts:AssumeRole"
      Effect    = "Allow"
      Principal = { Service = "ec2.amazonaws.com" }
    }]
  })

  tags = local.common_tags
}

resource "aws_iam_role_policy" "bench_ecr_pull" {
  name = "ecr-pull"
  role = aws_iam_role.bench.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ecr:GetAuthorizationToken",
        ]
        Resource = "*"
      },
      {
        Effect = "Allow"
        Action = [
          "ecr:BatchGetImage",
          "ecr:GetDownloadUrlForLayer",
          "ecr:BatchCheckLayerAvailability",
        ]
        Resource = [
          module.ecr_serde_bench_server.ecr-repository-arn,
          module.ecr_axum_serde_server.ecr-repository-arn,
        ]
      }
    ]
  })
}

resource "aws_iam_instance_profile" "bench" {
  name = "harrow-bench"
  role = aws_iam_role.bench.name

  tags = local.common_tags
}
