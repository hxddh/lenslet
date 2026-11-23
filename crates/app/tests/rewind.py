"""Phases of a session against Lenslet, and a dump of what the account holds.

python rewind.py one   -> build things (S3 -> Lambda -> DynamoDB chain included)
python rewind.py two   -> change them
python rewind.py dump  -> print the account's contents as JSON
"""
import io, json, os, sys, time, zipfile

import boto3
from botocore.config import Config

c = dict(endpoint_url=os.environ["AWS_ENDPOINT_URL"], region_name="us-east-1",
         aws_access_key_id="x", aws_secret_access_key="x", config=Config(retries={"max_attempts": 1}))
s3, ddb, sqs, lam, iam, sns = (boto3.client(n, **c) for n in ("s3", "dynamodb", "sqs", "lambda", "iam", "sns"))

CODE = '''
import boto3
ddb = boto3.client("dynamodb")
def handler(event, context):
    for r in event.get("Records", []):
        ddb.put_item(TableName="images", Item={"id": {"S": r["s3"]["object"]["key"]}})
'''

def one():
    ddb.create_table(TableName="orders", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    ddb.create_table(TableName="images", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    for t in ("orders", "images"):
        ddb.get_waiter("table_exists").wait(TableName=t, WaiterConfig={"Delay": 1})
    for i in range(3):
        ddb.put_item(TableName="orders", Item={"id": {"S": f"o-{i}"}, "total": {"N": str(i * 10)}})
    ddb.update_item(TableName="orders", Key={"id": {"S": "o-1"}}, UpdateExpression="SET paid = :t",
                    ExpressionAttributeValues={":t": {"BOOL": True}})
    s3.create_bucket(Bucket="assets")
    s3.put_object(Bucket="assets", Key="a.txt", Body=b"alpha")
    s3.put_object(Bucket="assets", Key="b.bin", Body=bytes(range(256)))
    q = sqs.create_queue(QueueName="jobs")["QueueUrl"]
    sqs.send_message(QueueUrl=q, MessageBody="job 1")
    sns.create_topic(Name="news")
    role = iam.create_role(RoleName="fn-role", AssumeRolePolicyDocument=json.dumps({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Principal": {"Service": "lambda.amazonaws.com"}, "Action": "sts:AssumeRole"}]}))["Role"]["Arn"]
    iam.put_role_policy(RoleName="fn-role", PolicyName="ddb", PolicyDocument=json.dumps({"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": "dynamodb:PutItem", "Resource": "*"}]}))
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr("app.py", CODE)
    fn = lam.create_function(FunctionName="index", Runtime="python3.12", Role=role, Handler="app.handler",
                             Code={"ZipFile": buf.getvalue()})["FunctionArn"]
    lam.get_waiter("function_active_v2").wait(FunctionName="index", WaiterConfig={"Delay": 1})
    s3.create_bucket(Bucket="uploads")
    lam.add_permission(FunctionName="index", StatementId="s3", Action="lambda:InvokeFunction",
                       Principal="s3.amazonaws.com", SourceArn="arn:aws:s3:::uploads")
    s3.put_bucket_notification_configuration(Bucket="uploads", NotificationConfiguration={
        "LambdaFunctionConfigurations": [{"LambdaFunctionArn": fn, "Events": ["s3:ObjectCreated:*"]}]})
    s3.put_object(Bucket="uploads", Key="cat.jpg", Body=b"meow")
    time.sleep(2)

def two():
    ddb.delete_item(TableName="orders", Key={"id": {"S": "o-0"}})
    ddb.put_item(TableName="orders", Item={"id": {"S": "o-9"}})
    s3.put_object(Bucket="assets", Key="c.txt", Body=b"gamma")
    s3.delete_object(Bucket="assets", Key="a.txt")
    q = sqs.get_queue_url(QueueName="jobs")["QueueUrl"]
    sqs.send_message(QueueUrl=q, MessageBody="job 2")
    s3.put_object(Bucket="uploads", Key="dog.jpg", Body=b"woof")
    ddb.create_table(TableName="extra", KeySchema=[{"AttributeName": "id", "KeyType": "HASH"}],
                     AttributeDefinitions=[{"AttributeName": "id", "AttributeType": "S"}], BillingMode="PAY_PER_REQUEST")
    ddb.get_waiter("table_exists").wait(TableName="extra", WaiterConfig={"Delay": 1})
    time.sleep(2)

def dump():
    out = {"tables": {}, "buckets": {}, "queues": {}, "topics": [], "functions": [], "roles": {}}
    for t in sorted(ddb.list_tables()["TableNames"]):
        items = ddb.scan(TableName=t)["Items"]
        out["tables"][t] = sorted(json.dumps(i, sort_keys=True) for i in items)
    for b in sorted(x["Name"] for x in s3.list_buckets()["Buckets"]):
        objs = s3.list_objects_v2(Bucket=b).get("Contents", [])
        out["buckets"][b] = {o["Key"]: s3.get_object(Bucket=b, Key=o["Key"])["Body"].read().hex() for o in objs}
    for u in sorted(sqs.list_queues().get("QueueUrls", [])):
        a = sqs.get_queue_attributes(QueueUrl=u, AttributeNames=["ApproximateNumberOfMessages"])["Attributes"]
        out["queues"][u.rsplit("/", 1)[-1]] = a["ApproximateNumberOfMessages"]
    out["topics"] = sorted(t["TopicArn"] for t in sns.list_topics()["Topics"])
    out["functions"] = sorted(f["FunctionName"] for f in lam.list_functions()["Functions"])
    for r in iam.list_roles()["Roles"]:
        out["roles"][r["RoleName"]] = sorted(iam.list_role_policies(RoleName=r["RoleName"])["PolicyNames"])
    print(json.dumps(out, sort_keys=True))

{"one": one, "two": two, "dump": dump}[sys.argv[1]]()
