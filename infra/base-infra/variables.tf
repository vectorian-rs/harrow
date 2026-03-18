variable "region" {
  description = "AWS region"
  type        = string
  default     = "eu-west-1"
}

variable "availability_zone" {
  description = "AWS availability zone for both benchmark nodes (empty = auto-select first available AZ)"
  type        = string
  default     = "eu-west-1b"
}
