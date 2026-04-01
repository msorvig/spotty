use aws_codegen::{GenerateSpec, Protocol};

fn main() {
    GenerateSpec {
        name:          "ec2",
        protocol:      Protocol::Query,
        runtime_crate: "aws_runtime_async",
        operations:    &[
            "DescribeInstanceTypes",
            "DescribeSpotPriceHistory",
            "RunInstances",
            "TerminateInstances",
            "DescribeInstances",
            "DescribeKeyPairs",
            "CreateKeyPair",
            "DescribeSecurityGroups",
            "DescribeVpcs",
            "CreateSecurityGroup",
            "AuthorizeSecurityGroupIngress",
            "DescribeImages",
            "CreateImage",
            "DeregisterImage",
            "DeleteSnapshot",
        ],
        sync: false,
    }.generate();

    GenerateSpec {
        name:          "ssm",
        protocol:      Protocol::Json,
        runtime_crate: "aws_runtime_async",
        operations:    &[
            "GetParameter",
            "GetParameters",
        ],
        sync: false,
    }.generate();

    GenerateSpec {
        name:          "iam",
        protocol:      Protocol::Query,
        runtime_crate: "aws_runtime_async",
        operations:    &[
            "GetUser",
            "CreateUser",
            "PutUserPolicy",
            "ListAccessKeys",
            "DeleteAccessKey",
            "CreateAccessKey",
            "CreateRole",
            "GetRole",
            "AttachRolePolicy",
        ],
        sync: false,
    }.generate();

    GenerateSpec {
        name:          "lambda",
        protocol:      Protocol::RestJson,
        runtime_crate: "aws_runtime_async",
        operations:    &[
            "CreateFunction",
            "DeleteFunction",
            "GetFunction",
            "GetFunctionConfiguration",
            "Invoke",
            "ListFunctions",
            "UpdateFunctionCode",
            "UpdateFunctionConfiguration",
            "TagResource",
            "ListTags",
        ],
        sync: false,
    }.generate();

    GenerateSpec {
        name:          "sts",
        protocol:      Protocol::Query,
        runtime_crate: "aws_runtime_async",
        operations:    &[
            "GetCallerIdentity",
        ],
        sync: false,
    }.generate();
}
