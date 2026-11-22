data "aws_partition" "current" {}
data "aws_region" "current" {}
data "aws_caller_identity" "current" {}

data "aws_iam_policy_document" "lenslet" {
  statement {
    sid       = "StsAccess"
    effect    = "Allow"
    actions   = ["sts:GetSessionToken"]
    resources = ["*"]
  }
  statement {
    sid       = "ApigatewayRestapis"
    effect    = "Allow"
    actions   = ["apigateway:GET"]
    resources = ["arn:${data.aws_partition.current.partition}:apigateway:${data.aws_region.current.region}::/restapis/*"]
  }
  statement {
    sid       = "BedrockUsAnthropicClaude3Haiku20240307V10"
    effect    = "Allow"
    actions   = ["bedrock:InvokeModel"]
    resources = ["arn:${data.aws_partition.current.partition}:bedrock:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:inference-profile/us.anthropic.claude-3-haiku-20240307-v1:0"]
  }
  statement {
    sid       = "BedrockAnthropicClaude3Haiku20240307V10"
    effect    = "Allow"
    actions   = ["bedrock:InvokeModel"]
    resources = ["arn:${data.aws_partition.current.partition}:bedrock:*::foundation-model/anthropic.claude-3-haiku-20240307-v1:0"]
  }
  statement {
    sid       = "DynamodbOrders"
    effect    = "Allow"
    actions   = ["dynamodb:GetItem", "dynamodb:PutItem"]
    resources = ["arn:${data.aws_partition.current.partition}:dynamodb:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:table/orders"]
  }
  statement {
    sid       = "IamWorker"
    effect    = "Allow"
    actions   = ["iam:PassRole"]
    resources = ["arn:${data.aws_partition.current.partition}:iam::${data.aws_caller_identity.current.account_id}:role/service-role/worker"]
  }
  statement {
    sid       = "KmsKey"
    effect    = "Allow"
    actions   = ["kms:Decrypt"]
    resources = ["arn:${data.aws_partition.current.partition}:kms:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:key/*"]
  }
  statement {
    sid       = "LambdaFunctionThumbnailerProd"
    effect    = "Allow"
    actions   = ["lambda:InvokeFunction"]
    resources = ["arn:${data.aws_partition.current.partition}:lambda:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:function:thumbnailer:prod"]
  }
  statement {
    sid       = "Route53Z123"
    effect    = "Allow"
    actions   = ["route53:GetHostedZone"]
    resources = ["arn:${data.aws_partition.current.partition}:route53:::hostedzone/Z123"]
  }
  statement {
    sid       = "S3ShopAssets"
    effect    = "Allow"
    actions   = ["s3:ListBucket"]
    resources = ["arn:${data.aws_partition.current.partition}:s3:::shop-assets"]
  }
  statement {
    sid       = "S3ShopAssets2"
    effect    = "Allow"
    actions   = ["s3:GetObject"]
    resources = ["arn:${data.aws_partition.current.partition}:s3:::shop-assets/*"]
  }
  statement {
    sid       = "SqsJobs"
    effect    = "Allow"
    actions   = ["sqs:SendMessage"]
    resources = ["arn:${data.aws_partition.current.partition}:sqs:${data.aws_region.current.region}:${data.aws_caller_identity.current.account_id}:jobs"]
  }
}
