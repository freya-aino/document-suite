import json
import os

from langchain_core.prompts import ChatPromptTemplate
from langchain_openai import ChatOpenAI

# ------------------------------------------------------------------------------
with open("./openai-api-key.txt", "r") as f:
    openai_api_key = f.read().strip()

os.environ["OPENAI_API_KEY"] = openai_api_key

# ------------------------------------------------------------------------------

from openai import OpenAI

def generate_image(prompt, size=(1024, 1024), quality="standard", n=1):
    
    client = OpenAI()
    
    response = client.images.generate(
        model="dall-e-3",
        prompt=prompt,
        size=f"{size[0]}x{size[1]}",
        quality=quality,
        n=n,
    )
    
    image_urls = [d.url for d in response.data]
    
    print(image_urls)

# ------------------------------------------------------------------------------

LLM = ChatOpenAI(
    model="gpt-4o-mini",
    temperature=0.1,
    max_tokens=None,
    timeout=None,
    max_retries=2
)

LLM_CHAT = ChatOpenAI(
    model="gpt-4o",
    temperature=0.05,
    max_tokens=None,
    timeout=None,
    max_retries=2
)


# ------------------------------------------------------------------------------

EXTRACTION_PROMPT = ChatPromptTemplate.from_messages(
    [
        (
            "system",
            "<instructions>{instructions}</instructions>"
        ),
        (
            "assistant",
            "<information>{information}</information>"
        )
    ]
)

CHAT_PROMPT = ChatPromptTemplate.from_messages(
    [
        (
            "system",
            "<instructions>{instructions}</instructions>"
        ),
        (
            "assistant",
            "<information>{information}</information>"
        ),
        (
            "user",
            "<prompt>{prompt}</prompt>"
        )
    ]
)

GENERATION_PROMPT = ChatPromptTemplate.from_messages(
    [
        (
            "system",
            "<instructions>{instructions}</instructions>"
        ),
        (
            "assistant",
            "<information>{information}</information>"
        ),
        (
            "system",
            "<chat_history>{chat_history}</chat_history>"
        ),
        (
            "user",
            "<prompt>{prompt}</prompt>"
        )
    ]
)

# ------------------------------------------------------------------------------

IMAGE_GENERATION_INSTRUCTIONS = """
you are an AI image prompt engineering expert.
you only generate AI Image generation prompts, you do not answer questions or engage in chat!

you will recieve a prompt request from the user to generate an image generation prompt.
The promp you generate needs to be in the format of modern SOTA image generation prompt standards.

The user uploads a set of images, and you will be given:
1) a set of information extracted from deep learning models about the images.
2) an expert assistants description and reasoning over each of the images information.
3) all prior chat messages between the user and the assistant.
You MUST include the assistants information and/or any raw information from the image set in the image prompt you generate.
This information includes but is not limited to: classifications, locations, poses, texts, effects, objects, etc.


you need to use the following specifications about the image generation to generate the prompt:
- your assistants have a good understanding of each piece of information, their assesstment should be considered accurate and reliable.
- only in edge cases, where information between assistants is conflicting, you should re-evaluate using the raw_assistant_information to provide answers.
- chat_history information contains additional context what is in the images and what should be generated.
- <information> provides two sets of information for each image in the collection:
    1) <assistant_information> - a set of proffessional assistants extracted usefull descriptions and information.
    2) <raw_assistant_information> - the raw information extracted directly from the images.
- the information provided is in the format:
    [<image=file_name>
        <assistant_information>
            <classification>...</classification>
            <object_detection>...</object_detection>
            <pose_estimation>...</pose_estimation>
            <text_extraction>...</text_extraction>
        </assistant_information>
        <raw_assistant_information>
            <classification>...</classification>
            <object_detection>...</object_detection>
            <pose_estimation>...</pose_estimation>
            <text_extraction>...</text_extraction>
        </raw_assistant_information>
    </image>, <image=file_name>...</image>, ...]
- the designated information is in the format:
    <classification>...</classification> - classification information extracted from the image
    <object_detection>...</object_detection> - object detection information extracted from the image
    <pose_estimation>...</pose_estimation> - pose estimation information extracted from the image
    <text_extraction>...</text_extraction> - text extraction information extracted from the image
"""

CHAT_INSTRUCTIONS = """
you are charlie, a personal, ready to help, sevice chatbot to help the user with their prompts.
The user uploads a set of images that they wish to talk and inqurry information about.

Reason over each of the images information and provide the user with a personal answer to their questions.
Answer in a polite, professional but kind manner, and explain why you are providing the information you are providing.
Keep the answers short and differentiate between the user asking:
- a techical (extraction) question, for example "how many people are there accross all the images?" or "how many document images are there ?"
- or a general (explanation) question, for example "what motives do we have in the images?" or "what are the images about?"

you need to use the following specifications about the chat to answer the questions and describe the information:
- your assistants have a good understanding of each piece of information, their assesstment should be considered accurate and reliable.
- only in edge cases, where information between assistants is conflicting, you should re-evaluate using the raw_assistant_information to provide answers.
- <information> provides two sets of information for each image in the collection:
    1) <assistant_information> - a set of proffessional assistants extracted usefull descriptions and information.
    2) <raw_assistant_information> - the raw information extracted directly from the images.
- the information provided is in the format:
    [<image=file_name>
        <assistant_information>
            <classification>...</classification>
            <object_detection>...</object_detection>
            <pose_estimation>...</pose_estimation>
            <text_extraction>...</text_extraction>
        </assistant_information>
        <raw_assistant_information>
            <classification>...</classification>
            <object_detection>...</object_detection>
            <pose_estimation>...</pose_estimation>
            <text_extraction>...</text_extraction>
        </raw_assistant_information>
    </image>, <image=file_name>...</image>, ...]
- the designated information is in the format:
    <classification>...</classification> - classification information extracted from the image
    <object_detection>...</object_detection> - object detection information extracted from the image
    <pose_estimation>...</pose_estimation> - pose estimation information extracted from the image
    <text_extraction>...</text_extraction> - text extraction information extracted from the image
- not all information is available for all images, so some information might be missing, for example an image without persons, pose_estimation_information should be empty.
"""

OCR_INSTRUCTIONS = """
you are a optical character recognition (ocr) parsing expert.
in a 1 paragraph answer describe the information received.
reason over the ocr text received and the word bits to answer questions about:
how many seperate texts there are, what the texts are about, where the texts are in the image and what type of texts it is (document, receipt, billboard, etc).

Text extraction might be missleading, so reason over the position and location of each word bit to determine if the text is a document or appears in a non document image.
If you determine it is most likely a document you should list all available information in the structure you think is displayed: if it is a list, a table, a paragraph, etc..

you need to use the following specifications about the ocr to answer the questions and describe the information:
- the ocr text is in the format: ocr_text=text.
- the ocr text is the result of ocr being performed on the image, so spacing and formatting is not reliable.
- when ocr is applied to images without text, the ocr will still return text, but it will be not legible and not useable.
- the ocr word informations are in the format: ocr_word_bits=words.
- each element in the ocr_word_bits has the following data:
    - text: the text of the word bit.
    - bbox: the bounding box of the word bit in the format: [x, y, w, h] where x and y are the top left corner of the bounding box, and w and h are the width and height of the bounding box.
    - confidence: confidence of the word being detected between 0 and 1, where 1 is the highest confidence.
"""

OBJECT_DETECTION_INSTRUCTIONS = """
you are a object detection parsing expert.
in a 1 paragraph answer describe the information received.
reason over the objects names, their locations in the image, their size and the confidence of their prediction to answer quetions about: 
what the objects are doing, where they are in te image and how they might be interacting with each other.

you need to use the following specifications about the objects to answer the questions and describe the information:
- the objects are designated by their class name designated by class=class_name
- the bounding box of the object is in the format: bbox=[x, y, w, h] where x and y are the top left corner of the bounding box, and w and h are the width and height of the bounding box
- the x, y and w, h values are between 0 and 1 and represent the position and size of the bounding box in the image
- image coordinates are in the format: x=0 is the left side of the image, x=1 is the right side of the image, y=0 is the top of the image, y=1 is the bottom of the image
- the confidence values are between 0 and 1 and represent the confidence of the object being detected
"""

POSE_INSTRUCTIONS = """
you are a image pose parsing expert. 
in a 1 paragraph answer describe the information received. 
reason over all keypoints and the confidence of each keypoint and answer questions about: 
how many people there are, each persons pose, where the persons are in the image and what they might be doing.

you need to use the following specifications about the pose to answer the questions and describe the information:
- the keypoints are in the format: name=[x, y] listed in the keypoints list
- each keypoint has a confidence value formatted as name=confidence
- the keypoint values are between 0 and 1 and represent the position of the keypoint in the image
- the directions of the image relative to the keypoints are: x=0 is the left side of the image, x=1 is the right side of the image, y=0 is the top of the image, y=1 is the bottom of the image
- the confidence values are between 0 and 1 and represent the confidence of the keypoint being detected
"""

CLASSIFICATION_INSTRUCTIONS = """
you are a image classification parsing expert. 
in a 1 paragraph answer describe the information received.
reson over all lemmas and probabilities and answer questions about:
what the image is about, what type of image or document it is and what the different probabilities indicate the image content to be.

you need to use the following specifications about the classification to answer the questions and describe the information:
- each row is a classification result with the lemmas and probabilities
- the lemmas are in the format: lemmas=lemma1, lemma2, ..., lemmaN and denote a list of possible words to describe that image classification
- the information starts with the dataset name used for classification
- the probabilities are in the format: probability=0.0 and denote the probability of that classification being correct
- when classification probabilities are spread out over multiple classes, the image is likely to be a mix of those classes.
- similar classes might appear as multiple slightly different individual classes and probabilities, this can make their combined probability higher that other classes.
"""




# ------------------------------------------------------------------------------





def classification_to_prompt(classification_response):
    out = "\n".join([
        f"lemmas= {', '.join(l['lemmas'])} - probability: {p:.2f}"
        for p, l in zip(classification_response["probabilities"], classification_response["labels"])
    ])
    return "imagenet 21k image classification:\n" + out

def pose_to_prompt(pose_response):
    keypoint_names = ["Nose", "Left Eye", "Right Eye", "Left Ear", "Right Ear", "Left Shoulder", "Right Shoulder", "Left Elbow", "Right Elbow", "Left Wrist", "Right Wrist", "Left Hip", "Right Hip", "Left Knee", "Right Knee", "Left Ankle", "Right Ankle"]
    out = ""
    for (i, person_landmarks) in enumerate(pose_response["result"][0]["landmarks"]):
        out += f"person {i}:\n"
        out += "keypoints: " + ", ".join(f"{name}={kpt}" for (name, kpt) in zip(keypoint_names, person_landmarks["keypoints"][0])) + "\n"
        out += "confidences: " + ", ".join(f"{name}={kpt}" for (name, kpt) in zip(keypoint_names, person_landmarks["confidence"][0])) + "\n"
    return "human poses:\n" + out

def objects_to_prompt(objects_response):
    out = ""
    for object_ in objects_response["result"][0]["bboxes"]:
        out += f"class={object_['class']}" + "\n"
        out += f"bbox={object_['bbox']}" + "\n"
        out += f"confidence={object_['confidence']}" + "\n"
    return "objects:\n" + out

def ocr_to_prompt(ocr_response_text, ocr_response_data):
    out = "ocr_text:\n" + ocr_response_text["text"] + "\n"
    out += "ocr_word_bits:\n" + json.dumps(ocr_response_data["words"]) + "\n"
    return "optical_character_recognition (ocr):\n" + out

def get_prompt_formatting(responses):
    return {
        "classification": classification_to_prompt(responses["classification"]) if responses["classification"] else None,
        "object_detection": objects_to_prompt(responses["object_detection"]) if responses["object_detection"] else None,
        "pose_estimation": pose_to_prompt(responses["pose_estimation"]) if responses["pose_estimation"] else None,
        "text_extraction": ocr_to_prompt(responses["text_extraction"], responses["text_data_extraction"]) if responses["text_extraction"] and responses["text_data_extraction"] else None
    }

def gpt_extract_information(responses, message_column):
    
    classification_results = None
    object_detection_results = None
    pose_estimation_results = None
    text_extraction_results = None
    
    chain = EXTRACTION_PROMPT | LLM
    
    formatted_prompts = get_prompt_formatting(responses)
    
    if formatted_prompts["classification"]:
        try:
            classification_results = chain.invoke({
                "instructions": CLASSIFICATION_INSTRUCTIONS, 
                "information": formatted_prompts["classification"]
            }).content
            message_column.success("LLM info for classification")
        except Exception as e:
            message_column.warning(f"Error: {e}")
        
    if formatted_prompts["object_detection"]:
        try:
            object_detection_results = chain.invoke({
                "instructions": OBJECT_DETECTION_INSTRUCTIONS, 
                "information": formatted_prompts["object_detection"]
            }).content
            message_column.success("LLM info for object detection")
        except Exception as e:
            message_column.warning(f"Error: {e}")
        
    if formatted_prompts["pose_estimation"]:
        try:
            pose_estimation_results = chain.invoke({
                "instructions": POSE_INSTRUCTIONS, 
                "information": formatted_prompts["pose_estimation"]
            }).content
            message_column.success("LLM info for pose estimation")
        except Exception as e:
            message_column.warning(f"Error: {e}")
        
    if formatted_prompts["text_extraction"]:
        try:
            text_extraction_results = chain.invoke({
                "instructions": OCR_INSTRUCTIONS, 
                "information": formatted_prompts["text_extraction"]
            }).content
            message_column.success("LLM info for text extraction")
        except Exception as e:  
            message_column.warning(f"Error: {e}")
    
    return {
        "classification": classification_results,
        "object_detection": object_detection_results,
        "pose_estimation": pose_estimation_results,
        "text_extraction": text_extraction_results
    }

def assistant_chat_api(user_input, extracted_information, gpt_extracted_information):
    
    information = ""
    for file_name in gpt_extracted_information:
        gpt_information = gpt_extracted_information[file_name]
        ext_information = extracted_information[file_name]
        
        for k in gpt_information:
            gpt_information[k] = gpt_information[k].replace("\"", "'") if gpt_information[k] else ""
        
        ext_information_prompts = get_prompt_formatting(ext_information)
        
        information += f"""
            <image={file_name}>
                <assistant_information>
                    <classification>{gpt_information.get('classification', '')}</classification>
                    <object_detection>{gpt_information.get('object_detection', '')}</object_detection>
                    <pose_estimation>{gpt_information.get('pose_estimation', '')}</pose_estimation>
                    <text_extraction>{gpt_information.get('text_extraction', '')}</text_extraction>
                </assistant_information>
                <raw_assistant_information>
                    <classification>{ext_information_prompts.get('classification', '')}</classification>
                    <object_detection>{ext_information_prompts.get('object_detection', '')}</object_detection>
                    <pose_estimation>{ext_information_prompts.get('pose_estimation', '')}</pose_estimation>
                    <text_extraction>{ext_information_prompts.get('text_extraction', '')}</text_extraction>
                </raw_assistant_information>
            </image>
            """
    
    chain = CHAT_PROMPT | LLM_CHAT
    
    answer = chain.invoke({
        "instructions": CHAT_INSTRUCTIONS,
        "information": information,
        "prompt": user_input
    }).content
    
    return answer

def image_generator_api(user_input, extracted_information, gpt_extracted_information, chat_history):
    
    information = ""
    for file_name in gpt_extracted_information:
        gpt_information = gpt_extracted_information[file_name]
        ext_information = extracted_information[file_name]
        
        for k in gpt_information:
            gpt_information[k] = gpt_information[k].replace("\"", "'") if gpt_information[k] else ""
        
        ext_information_prompts = get_prompt_formatting(ext_information)
        
        information += f"""
            <image={file_name}>
                <assistant_information>
                    <classification>{gpt_information.get('classification', '')}</classification>
                    <object_detection>{gpt_information.get('object_detection', '')}</object_detection>
                    <pose_estimation>{gpt_information.get('pose_estimation', '')}</pose_estimation>
                    <text_extraction>{gpt_information.get('text_extraction', '')}</text_extraction>
                </assistant_information>
                <raw_assistant_information>
                    <classification>{ext_information_prompts.get('classification', '')}</classification>
                    <object_detection>{ext_information_prompts.get('object_detection', '')}</object_detection>
                    <pose_estimation>{ext_information_prompts.get('pose_estimation', '')}</pose_estimation>
                    <text_extraction>{ext_information_prompts.get('text_extraction', '')}</text_extraction>
                </raw_assistant_information>
            </image>
            """
    
    chat_history = "".join([f"<chat><role>{message['role']}</role><content>{message['content']}</content></chat>" for message in chat_history])
    
    chain = GENERATION_PROMPT | LLM_CHAT
    
    image_prompt = chain.invoke({
        "instructions": IMAGE_GENERATION_INSTRUCTIONS,
        "information": information,
        "chat_history": chat_history,
        "prompt": user_input
    }).content
    
    return image_prompt
