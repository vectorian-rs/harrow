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

data "aws_caller_identity" "current" {}

data "aws_region" "current" {}

data "aws_availability_zones" "available" {
  state = "available"
}

# Caller's public IP for SSH access
data "http" "my_ip" {
  url = "https://checkip.amazonaws.com"
}

locals {
  my_ip = "${trimspace(data.http.my_ip.response_body)}/32"
  az    = var.availability_zone != "" ? var.availability_zone : data.aws_availability_zones.available.names[0]

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

module "ecr_harrow_perf_server" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/harrow-perf-server"
  stage           = "bench"
  force-delete    = true
}

module "ecr_axum_perf_server" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/axum-perf-server"
  stage           = "bench"
  force-delete    = true
}

module "ecr_harrow_perf_server_sysalloc" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/harrow-perf-server-sysalloc"
  stage           = "bench"
  force-delete    = true
}

module "ecr_axum_perf_server_sysalloc" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/axum-perf-server-sysalloc"
  stage           = "bench"
  force-delete    = true
}

module "ecr_spinr" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/spinr"
  stage           = "bench"
  force-delete    = true
}

module "ecr_harrow_monoio_server" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/harrow-monoio-server"
  stage           = "bench"
  force-delete    = true
}

module "ecr_vegeta" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/vegeta"
  stage           = "bench"
  force-delete    = true
}

module "ecr_load_generators" {
  source          = "s3::https://s3-eu-west-1.amazonaws.com/datadeft-tf-modules/components/ecr-v1.0.0.zip"
  repository-name = "harrow/load-generators"
  stage           = "bench"
  force-delete    = true
}

# ---------------------------------------------------------------------------
# IAM — ECR pull for bench instances
# ---------------------------------------------------------------------------

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
          module.ecr_harrow_perf_server.ecr-repository-arn,
          module.ecr_axum_perf_server.ecr-repository-arn,
          module.ecr_harrow_perf_server_sysalloc.ecr-repository-arn,
          module.ecr_axum_perf_server_sysalloc.ecr-repository-arn,
          module.ecr_spinr.ecr-repository-arn,
          module.ecr_harrow_monoio_server.ecr-repository-arn,
          module.ecr_vegeta.ecr-repository-arn,
          module.ecr_load_generators.ecr-repository-arn,
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
