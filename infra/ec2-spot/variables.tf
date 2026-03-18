variable "region" {
  description = "AWS region"
  type        = string
  default     = "eu-west-1"
}

variable "instance_type" {
  description = "EC2 instance type (ARM64 Graviton recommended)"
  type        = string
  default     = "c8gn.12xlarge"
}

variable "key_name" {
  description = "Name of the SSH key pair in AWS"
  type        = string
}

variable "repo_url" {
  description = "Git repository URL to clone on instances"
  type        = string
  default     = "https://github.com/l1x/harrow.git"
}

variable "branch" {
  description = "Git branch to checkout"
  type        = string
  default     = "main"
}

variable "spot_price" {
  description = "Max spot price (empty string = on-demand price cap)"
  type        = string
  default     = ""
}
