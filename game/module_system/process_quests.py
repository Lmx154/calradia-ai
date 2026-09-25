
from module_info import *
from module_quests import *

from process_common import *

def save_quests():
  ofile = open(export_dir + "quests.txt","w", encoding="cp1254")
  ofile.write("questsfile version 1\n")
  ofile.write("%d\n"%(len(quests)))
  for i_quest in range(len(quests)):
    quest = quests[i_quest]
    ofile.write("qst_%s %s %d "%(quest[0],(quest[1].replace(" ","_")),quest[2]))
    ofile.write("%s "%(quest[3].replace(" ","_")))
    ofile.write("\n")
  ofile.close()

def save_python_header():
  ofile = open("./ID_quests.py","w", encoding="cp1254")
  for i_quest in range(len(quests)):
    ofile.write("qst_%s = %d\n"%(quests[i_quest][0],i_quest))
  for i_quest in range(len(quests)):
    ofile.write("qsttag_%s = %d\n"%(quests[i_quest][0],opmask_quest_index|i_quest))
  ofile.write("\n\n")
  ofile.close()


print("Exporting quest data...")
save_quests()
save_python_header()
  
