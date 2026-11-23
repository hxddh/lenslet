"""A small serverless app against Lenslet: S3 -> Lambda -> DynamoDB,
SNS -> SQS, EventBridge -> SQS."""
import io, json, os, time, zipfile

import boto3
from botocore.config import Config

c = dict(
    endpoint_url=os.environ["AWS_ENDPOINT_URL"],
    region_name="us-east-1",
    aws_access_key_id="x",
    aws_secret_access_key="x",
    config=Config(retries={"max_attempts": 1}),
)
s3, ddb, lam, iam = (boto3.client(n, **c) for n in ("s3", "dynamodb", "lambda", "iam"))
sns, sqs, events = (boto3.client(n, **c) for n in ("sns", "sqs", "events"))

CODE = '''
import boto3
ddb = boto3.client("dynamodb")
def handler(event, context):
    for r in event.get("Records", []):
        key = r["s3"]["object"]["key"]
        ddb.put_item(TableName="images", Item={"id": {"S": key}})
        print("stored", key)
    return {"ok": True}
'''
buf = io.BytesIO()
with zipfile.ZipFile(buf, "w") as z:
    z.writestr("app.py", CODE)

role = iam.create_role(
    RoleName="resize-role",
    AssumeRolePolicyDocument=json.dumps({
        "Version": "2012-10-17",
        "Statement": [{"Effect": "Allow", "Principal": {"Service": "lambda.amazonaws.com"}, "Action": "sts:AssumeRole"}],
    }),
)["Role"]["Arn"]
ddb.create_table(TableName="images", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                 AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
ddb.get_waiter("table_exists").wait(TableName="images", WaiterConfig={"Delay": 1})
fn = lam.create_function(FunctionName="resize", Runtime="python3.12", Role=role, Handler="app.handler",
                         Code={"ZipFile": buf.getvalue()})["FunctionArn"]
lam.get_waiter("function_active_v2").wait(FunctionName="resize", WaiterConfig={"Delay": 1})
s3.create_bucket(Bucket="uploads")
lam.add_permission(FunctionName="resize", StatementId="s3", Action="lambda:InvokeFunction",
                   Principal="s3.amazonaws.com", SourceArn="arn:aws:s3:::uploads")
s3.put_bucket_notification_configuration(Bucket="uploads", NotificationConfiguration={
    "LambdaFunctionConfigurations": [{"LambdaFunctionArn": fn, "Events": ["s3:ObjectCreated:*"]}]})
s3.put_object(Bucket="uploads", Key="cat.jpg", Body=b"meow")

topic = sns.create_topic(Name="orders")["TopicArn"]
q = sqs.create_queue(QueueName="order-copies")["QueueUrl"]
qarn = sqs.get_queue_attributes(QueueUrl=q, AttributeNames=["QueueArn"])["Attributes"]["QueueArn"]
sns.subscribe(TopicArn=topic, Protocol="sqs", Endpoint=qarn)
sns.publish(TopicArn=topic, Message="order 1")

events.put_rule(Name="big-orders", EventPattern=json.dumps({"source": ["shop"]}))
events.put_targets(Rule="big-orders", Targets=[{"Id": "q", "Arn": qarn}])
events.put_events(Entries=[{"Source": "shop", "DetailType": "order", "Detail": "{}"}])

time.sleep(3)
print("items:", ddb.scan(TableName="images")["Count"])
print("messages:", sqs.get_queue_attributes(QueueUrl=q, AttributeNames=["ApproximateNumberOfMessages"])["Attributes"])
