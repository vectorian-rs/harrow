output "security_group_id" {
  description = "Security group ID for bench instances"
  value       = aws_security_group.bench.id
}

output "iam_instance_profile_name" {
  description = "IAM instance profile name for bench instances"
  value       = aws_iam_instance_profile.bench.name
}

output "placement_group_name" {
  description = "Placement group name for bench instances"
  value       = aws_placement_group.bench.name
}

output "availability_zone" {
  description = "Availability zone for bench instances"
  value       = local.az
}

output "ecr_harrow_perf_server_url" {
  description = "ECR repo URL for harrow-perf-server"
  value       = module.ecr_harrow_perf_server.ecr-repository-url
}

output "ecr_axum_perf_server_url" {
  description = "ECR repo URL for axum-perf-server"
  value       = module.ecr_axum_perf_server.ecr-repository-url
}

output "ecr_spinr_url" {
  description = "ECR repo URL for spinr (load tester)"
  value       = module.ecr_spinr.ecr-repository-url
}

output "ecr_registry" {
  description = "ECR registry URL prefix"
  value       = "${data.aws_caller_identity.current.account_id}.dkr.ecr.${data.aws_region.current.name}.amazonaws.com"
}
