import json
import os

from langchain_core.prompts import ChatPromptTemplate
from langchain_openai import ChatOpenAI

# ------------------------------------------------------------------------------
with open("./openai-api-key.txt", "r") as f:
    openai_api_key = f.read().strip()

os.environ["OPENAI_API_KEY"] = openai_api_key

# ------------------------------------------------------------------------------

def classification_to_prompt(classification_response):
    
    out = "\n".join([
        f"lemmas: {', '.join(l['lemmas'])} - probability: {p:.2f}"
        for p, l in zip(classification_response["probabilities"], classification_response["labels"])
    ])
    
    return (
        "you are a image classification parsing expert. in a 1 paragraph answer describe the information about lemmas and probabilities received", 
        "imagenet 21k image classification:\n" + out
    )

def pose_to_prompt(pose_response):
    
    keypoint_names = ["Nose", "Left Eye", "Right Eye", "Left Ear", "Right Ear", "Left Shoulder", "Right Shoulder", "Left Elbow", "Right Elbow", "Left Wrist", "Right Wrist", "Left Hip", "Right Hip", "Left Knee", "Right Knee", "Left Ankle", "Right Ankle"]
    out = ""
    for (i, person_landmarks) in enumerate(pose_response["result"][0]["landmarks"]):
        out += f"person {i}:\n"
        out += "keypoints: " + ", ".join(f"{name}={kpt}" for (name, kpt) in zip(keypoint_names, person_landmarks["keypoints"][0])) + "\n"
        out += "confidences: " + ", ".join(f"{name}={kpt}" for (name, kpt) in zip(keypoint_names, person_landmarks["confidence"][0])) + "\n"
    
    # add instructions for the location in the image keypoints and bbxes are in , as well as the npumber of persons and objects of each type
    
    return (
        """
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
        """,
        "human poses:\n" + out
    )

def objects_to_prompt(objects_response):
    
    out = ""
    for object_ in objects_response["result"][0]["bboxes"]:
        out += f"class={object_["class"]}" + "\n"
        out += f"bbox={object_["bbox"]}" + "\n"
        out += f"confidence={object_["confidence"]}" + "\n"
    
    return (
        """
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
        """,
        "objects:\n" + out
    )

def ocr_to_prompt(ocr_response_text, ocr_response_data):
    
    out = "ocr_text:\n" + ocr_response_text["text"] + "\n"
    out += "ocr_word_bits:\n" + json.dumps(ocr_response_data["words"]) + "\n"
    
    return (
        """
        you are a optical character recognition (ocr) parsing expert.
        in a 1 paragraph answer describe the information received.
        reason over the ocr text received and the word bits to answer questions about:
        how many seperate texts there are, what the texts are about, where the texts are in the image and what type of texts it is (document, receipt, billboard, etc).
        
        Additionally you must  list all available information as coherently as possible, if it is a list, a table, a paragraph, etc. format it accordingly and designate it with your classification of what type of text it is.
        
        you need to use the following specifications about the ocr to answer the questions and describe the information:
        - the ocr text is in the format: ocr_text=text.
        - the ocr text is the result of ocr being performed on the image, so spacing and formatting is not reliable.
        - the ocr word informations are in the format: ocr_word_bits=words.
        - each element in the ocr_word_bits has the following data:
            - text: the text of the word bit.
            - bbox: the bounding box of the word bit in the format: [x, y, w, h] where x and y are the top left corner of the bounding box, and w and h are the width and height of the bounding box.
            - confidence: confidence of the word being detected between 0 and 1, where 1 is the highest confidence.
        """,
        "optical_character_recognition (ocr):\n" + out
    )



def generate_prompt(image_description):
    classification = image_description.get('classification', 'unknown')
    objects = image_description.get('objects', [])
    key_points = image_description.get('key_points', {})

    prompt = f"This image is classified as a '{classification}'."
    
    if objects:
        prompt += " It contains the following objects: "
        for obj in objects:
            label = obj.get('label', 'unknown object')
            bbox = obj.get('bounding_box', [])
            bbox_str = ", ".join(map(str, bbox))
            prompt += f"a '{label}' with bounding box coordinates {bbox_str}; "
    
    if key_points:
        prompt += "Key points detected are: "
        for point, coords in key_points.items():
            coords_str = ", ".join(map(str, coords))
            prompt += f"'{point}' at coordinates {coords_str}; "
    
    return prompt


if __name__ == "__main__":
    
    llm = ChatOpenAI(
        model="gpt-4o-mini",
        temperature=0,
        max_tokens=None,
        timeout=None,
        max_retries=2
    )
    
    prompt = ChatPromptTemplate.from_messages(
        [
            (
                "system",
                ""
            ),
            (
                "assistant",
                "{input}"
            )
        ]
    )
    
    chain = prompt | llm
    
    results = chain.invoke(
        {
            "input": "211 50668 9349253*  * Tel. 0221 9349253*  U ID Nr. : DE 812706034 EUR PERON I 0,0% SX 6.49 A PF AND 0,48 EUR 0.48 A*  TONIC WATER ZERO 3.58 A 2.5 k 1.79 PF AND 0.25 EUR 0.25 0.50 A*  2.5 k 0.25 ORANGE 0. SAFE BRAN G IN CH TE MIX AN AN. DIRE KT S AFT 2.58 A 2.79 SUM ME EUR 21,81 Ge g. EC- Cash EUR 21,81 * *  K unden be leg * *  Datum:  20.03.2024 U hr ze it:  12:56:06 U hr Be leg- Nr.  9741 Trace- Nr.  261921 Kart enz ahl ung Contact less giro card Nr.  56002712 Terminal- ID 00.075.00 Pas- Info AS- Zeit 20.03.  12:56 U hr Be trag EUR 21,81 Zah lung er fol gt Steuer %  Net to Steuer Brutto A= 19,0%  18,33 3,48 21,81 Ges amt be trag 18,33 3,48 21,81 TSE- Signatur:  D Kb Ry 6 PD 7 G 0 pJ p iS RuK/ p 59 fuRt k 7 XB 0 GD ZZ 67 sA 19 BY 0 rF on 080 oG d 3 i 6 T 0 dix G 0 ex 0 s Cs 50 uT i 5 KrB uZ yz Cu 6 Vl Pe 20 ut Hp y 0 Y W j k 4 YN Nb nYZ CD L 1 F Z S ira GU W j By TSE- Signatur zah ler:  3449663 TSE- Trans a ktion:  1675474 TSE- Start:  2024-03-20 T 12:53:39.000 TSE- Stop:  2024-03-20 T 12:54:48.000 Seri en nun mmer Kass e:  RE WE: d 8:5 e: d 3:48:3 f: e 3:00 20.03.2024 12:54 Bon- Nr. : 3089 Mark t: 0446 Kass e: 6 Bed. : 432106 No ch keine PAY BACK Kart e?  Fur dies enE in k auf hate st Du 10 Punk teer hal ten!  G le ichin der RE WE App oder auf www. re we. de/ pay back an mel den.  KeineR abatte oder Punk te auf mit*  gek enz eich net e Prod uk te.  RE WE Markt GmbH",
            "information": "items bought, item count, item price, total price, date, time, receipt number, payment method, market place"
        }
    )
    
    print(results)